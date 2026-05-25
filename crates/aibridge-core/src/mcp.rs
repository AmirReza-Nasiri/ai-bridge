//! MCP stdio server: newline-delimited JSON-RPC 2.0.
//!
//! Tool surface: `consult` (isolated, persisted topic dialogues), `implement`
//! (Codex returns a validated patch), `run` (structured command execution),
//! `review_diff` and the automatic `review_stop` gate (both share the warm Codex
//! peer's reserved review thread), plus `health` / `capability_status` /
//! `budget_status`. `review_diff` and `review_stop` capture the change set through
//! the same `git::diff_bundle` path so on-demand and automatic review judge an
//! identical set of changes.

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
    /// The reserved PRE-execution plan-gate thread (isolated from the review Gate
    /// so plan-dialogue history never anchors result reviews, or vice versa).
    PlanGate,
    /// A named, isolated consult dialogue.
    Consult(String),
}

/// Append a one-line note to a review result when a codex tool's elicitation had to
/// be declined this turn, so the human sees WHY a tool didn't run + can configure it.
fn append_elicit(text: String, note: Option<String>) -> String {
    match note {
        Some(n) => format!("{text}\n\n[AI Bridge: {n}]"),
        None => text,
    }
}

/// Human label for the live review-progress sink, from the thread being used.
fn progress_phase(key: &TopicKey) -> String {
    match key {
        TopicKey::Gate => "review".to_string(),
        TopicKey::PlanGate => "plan-gate".to_string(),
        TopicKey::Consult(name) => format!("consult:{name}"),
    }
}

/// Cap on simultaneously-tracked consult topics so a long session can't grow the
/// registry without bound. (The gate slot is separate and never evicted here.)
const MAX_CONSULT_TOPICS: usize = 32;

/// Reset the reserved review thread after this many reviews — bounds anchoring on
/// stale prior-diff findings while keeping warm-cache speed in between.
const GATE_RESET_EVERY: u32 = 10;

/// How long `ensure_peer` waits to ADOPT an in-flight background-warmed child before
/// giving up and cold-spawning. Sized to comfortably cover a cold child spawn + one
/// `xhigh` primer turn (the warm-up), so a first review that arrives mid-warm-up
/// adopts the warm child instead of paying the cold system-prompt cost again. The
/// Stop hook (1800s) and interactive tools tolerate this wait, which only happens
/// when warming is still in flight.
const WARM_ADOPT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

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
    /// The plan-gate epoch the PlanGate thread currently holds context for. When
    /// the on-disk epoch changes (a new task started), the thread is dropped so a
    /// new task's plan dialogue never anchors on the previous task's plan.
    plan_epoch: Option<String>,
}

impl Server {
    fn new() -> Self {
        Server {
            codex: None,
            warm_rx: None,
            threads: HashMap::new(),
            gate_reviews: 0,
            gates: HashMap::new(),
            plan_epoch: None,
        }
    }

    /// Ensure a live Codex child, preferring the background-warmed one (which
    /// arrives with a pre-warmed gate thread). Invariant: `self.threads` only ever
    /// holds threadIds for the CURRENT child, so it is cleared on every (re)spawn.
    fn ensure_peer(&mut self) -> anyhow::Result<()> {
        if self.codex.is_some() {
            return Ok(());
        }
        // Adopt the background-warmed child if one is in flight. BLOCK briefly
        // (bounded) rather than cold-spawning a second child: the warmer has already
        // paid the ~51K system-prompt cost on the Gate thread, so adopting it makes
        // the first review a warm `codex-reply` instead of paying that cost AGAIN on
        // a fresh cold child (the previous `try_recv` threw the in-flight warmer away
        // whenever the first review beat warm-up — the worst of both: cold + wasted).
        // This runs on the request thread, which is exactly the call that needs the
        // peer (it would otherwise wait on the cold spawn anyway); no lock is held.
        if let Some(rx) = self.warm_rx.take() {
            use std::sync::mpsc::RecvTimeoutError;
            match rx.recv_timeout(WARM_ADOPT_TIMEOUT) {
                Ok((peer, gate_tid)) => {
                    self.codex = Some(peer);
                    self.threads.clear();
                    self.threads.insert(TopicKey::Gate, gate_tid);
                    // Fresh child ⇒ fresh Gate thread: reset the anti-anchoring
                    // counter so a stale count can't drop this just-warmed thread
                    // before the next review uses it (Codex review).
                    self.gate_reviews = 0;
                    return Ok(());
                }
                // Warmer failed (thread ended → rx disconnected) or is too slow:
                // cold-spawn now. The abandoned warmer self-cleans — its later `send`
                // fails on the dropped rx and its CodexPeer is killed on Drop (its
                // open_thread is itself deadline-bounded by CALL_TIMEOUT).
                Err(RecvTimeoutError::Disconnected) | Err(RecvTimeoutError::Timeout) => {}
            }
        }
        self.codex = Some(CodexPeer::spawn()?);
        self.threads.clear();
        self.gate_reviews = 0; // new child ⇒ new Gate thread; reset the counter
        Ok(())
    }

    /// Start background warming unless a peer is already live OR a warmer is already
    /// in flight — idempotent, so re-warm calls never stack warmers/children.
    fn spawn_warming_if_absent(&mut self) {
        if self.codex.is_some() || self.warm_rx.is_some() {
            return;
        }
        self.warm_rx = Some(spawn_warming());
    }

    /// Drop the child + all its (now-dead) thread mappings so the next call spawns
    /// fresh. A timed-out/wedged child must never be reused — its stdin may no
    /// longer drain, and the next (synchronous, unbounded) write would then hang.
    /// Then re-warm in the background so the NEXT call can adopt a warm child rather
    /// than cold-spawn (otherwise one transport error returns us to cold first calls
    /// for the rest of the session).
    fn invalidate_peer(&mut self) {
        self.codex = None;
        self.threads.clear();
        // The current child's Gate + PlanGate threads died with it: reset the
        // per-child counters so a stale count can't drop the next child's
        // freshly-(re)warmed Gate thread, and so the next plan_gate re-opens cleanly.
        self.gate_reviews = 0;
        self.plan_epoch = None;
        self.spawn_warming_if_absent();
    }

    /// Ask Codex on `key`'s thread: continue it if known (warm `codex-reply`) or
    /// open a fresh one. A transport error invalidates the peer and propagates; a
    /// stale thread ("Session not found") transparently reopens.
    fn ask_topic(&mut self, key: TopicKey, prompt: &str, cwd: &str) -> anyhow::Result<String> {
        self.ensure_peer()?;
        let phase = progress_phase(&key);
        if let Some(tid) = self.threads.get(&key).cloned() {
            let (reply, elicit) = {
                let peer = self
                    .codex
                    .as_mut()
                    .ok_or_else(|| anyhow::anyhow!("codex peer unavailable"))?;
                peer.begin_progress(cwd, &phase);
                let r = peer.reply(&tid, prompt);
                let elicit = peer.last_elicitation_note(); // BEFORE end_progress drops the sink
                peer.end_progress(if r.is_ok() { "completed" } else { "error" });
                (r, elicit)
            };
            match reply {
                Ok(text) if !is_session_lost(&text) => return Ok(append_elicit(text, elicit)),
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
        let (opened, elicit) = {
            let peer = self
                .codex
                .as_mut()
                .ok_or_else(|| anyhow::anyhow!("codex peer unavailable"))?;
            peer.begin_progress(cwd, &phase);
            let r = peer.open_thread(prompt, cwd, crate::codex::REVIEW_REASONING_EFFORT);
            let elicit = peer.last_elicitation_note(); // BEFORE end_progress drops the sink
            peer.end_progress(if r.is_ok() { "completed" } else { "error" });
            (r, elicit)
        };
        match opened {
            Ok((tid, text)) => {
                if matches!(key, TopicKey::Consult(_)) {
                    self.evict_consult_if_full();
                }
                self.threads.insert(key, tid);
                Ok(append_elicit(text, elicit))
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
                "AI Bridge: no active review budget state yet (not implemented).".to_string()
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
            "implement" => {
                let task = msg
                    .pointer("/params/arguments/task")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .trim();
                if task.is_empty() {
                    "AI Bridge: `implement` requires a non-empty 'task' argument.".to_string()
                } else {
                    self.implement(task)
                }
            }
            "run" => {
                let command = msg
                    .pointer("/params/arguments/command")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .trim();
                let cwd = std::env::current_dir()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|_| ".".to_string());
                if command.is_empty() {
                    "AI Bridge: `run` requires a non-empty 'command' argument.".to_string()
                } else if crate::plan_gate::blocks_writes(&cwd) {
                    // In-tool defense: `run` executes arbitrary shell, so it must
                    // honor the plan gate even if the PreToolUse matcher missed it.
                    "AI Bridge: `run` is blocked by the plan gate — this task has no approved \
                     plan yet. Call `plan_gate` with your plan and retry after <AI-BRIDGE-APPROVE/>."
                        .to_string()
                } else if let Some(class) =
                    crate::plan_gate::unapproved_high_risk(&cwd, "mcp__aibridge__run", command)
                {
                    // Approved task, but a high-risk command class the plan didn't
                    // cover → re-arm the gate (same contract as the PreToolUse hook).
                    crate::plan_gate::revoke(&cwd, "high_risk_command_delta");
                    format!(
                        "AI Bridge: `run` blocked — {}",
                        crate::plan_gate::risk_delta_message(class)
                    )
                } else {
                    run_command(command)
                }
            }
            "plan_gate" => {
                let plan = msg
                    .pointer("/params/arguments/plan")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .trim();
                if plan.is_empty() {
                    "AI Bridge: `plan_gate` requires a non-empty 'plan' argument \
                     (todos, approach, intended_files, risk_surfaces, test_plan)."
                        .to_string()
                } else {
                    self.plan_gate(plan)
                }
            }
            "review_stop" => self.review_stop(msg),
            other => format!("AI Bridge: unknown tool '{other}'."),
        };
        json!({ "content": [{ "type": "text", "text": text }] })
    }

    /// On-demand second opinion on an isolated, named topic thread. A topic is
    /// REQUIRED. Continuous across calls AND across sessions: every topic's turns
    /// are persisted, so resuming it after a Claude restart replays recent context
    /// (codex threadIds don't survive a restart). `reset` archives the topic and
    /// starts cold.
    fn consult(&mut self, question: &str, topic: &str, reset: bool) -> String {
        let cwd = std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| ".".to_string());
        let topic = match normalize_topic(topic) {
            Ok(t) => t,
            Err(e) => return format!("AI Bridge: invalid consult topic — {e}"),
        };
        let key = TopicKey::Consult(topic.clone());
        if reset {
            self.threads.remove(&key);
            crate::topics::archive(&cwd, &topic); // keep history, start cold
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
        let resuming = !self.threads.contains_key(&key) && crate::topics::exists(&cwd, &topic);
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
                // Persist the RAW question + reply (not the seeded prompt).
                crate::topics::append_turn(&cwd, &topic, question, &reply);
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

    /// Ask Codex on a one-off EPHEMERAL thread (not tracked; isolated from consult
    /// topics and the gate). On error the peer is invalidated. Used by the
    /// implementer, whose calls must not pollute or anchor any persistent thread.
    /// The codex-internal thread lingers in the child (we discard its id), but is
    /// flushed on the next child restart (periodic gate reset / error invalidation).
    fn ask_ephemeral(&mut self, prompt: &str, cwd: &str) -> anyhow::Result<String> {
        self.ensure_peer()?;
        let opened = {
            let peer = self
                .codex
                .as_mut()
                .ok_or_else(|| anyhow::anyhow!("codex peer unavailable"))?;
            peer.begin_progress(cwd, "implement");
            let r = peer.open_thread(prompt, cwd, crate::codex::IMPLEMENT_REASONING_EFFORT);
            peer.end_progress(if r.is_ok() { "completed" } else { "error" });
            r
        };
        match opened {
            Ok((_tid, text)) => Ok(text), // discard threadId: ephemeral, never reused
            Err(e) => {
                self.invalidate_peer();
                Err(e)
            }
        }
    }

    /// Implementer: ask Codex for a single unified-diff patch implementing `task`,
    /// validate it (`git apply --check` when in a repo), retry once on failure, and
    /// return it for Claude to apply. Codex stays read-only — it PROPOSES; Claude
    /// applies; the Stop gate reviews afterwards. The patch is always PROPOSED/
    /// UNTESTED (a read-only peer can't run it).
    fn implement(&mut self, task: &str) -> String {
        // Claude-originated tool call: the server runs in the project dir, so
        // current_dir() is correct here (unlike the Stop hook, which needs
        // resolve_cwd for an unsubstituted ${cwd}). Same as review_diff.
        let cwd = std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| ".".to_string());
        let base = implement_prompt(task);
        // First attempt.
        let raw = match self.ask_ephemeral(&base, &cwd) {
            Ok(t) => t,
            Err(e) => return format!("AI Bridge: implement unavailable (Codex error): {e}."),
        };
        let patch = match patch_or_message(&raw) {
            Ok(p) => p,
            Err(msg) => return msg, // BLOCKED / NEEDS-INFO / no envelope
        };
        let why = match check_patch(&patch, &cwd) {
            PatchCheck::Applies => return format_patch(&patch, true),
            PatchCheck::Unchecked => return format_patch(&patch, false),
            PatchCheck::Rejected(why) => why,
        };
        // One retry, feeding back git's validator error.
        let retry = format!(
            "{base}\n\nYOUR PREVIOUS PATCH FAILED `git apply --check`:\n{why}\n\
             Return a corrected patch in the SAME envelope."
        );
        let raw2 = match self.ask_ephemeral(&retry, &cwd) {
            Ok(t) => t,
            Err(e) => return format!("AI Bridge: implement retry failed (Codex error): {e}."),
        };
        // patch_or_message on the RETRY too, so a BLOCKED/NEEDS-INFO answer to the
        // feedback is surfaced cleanly rather than as "no patch".
        let patch2 = match patch_or_message(&raw2) {
            Ok(p) => p,
            Err(msg) => return msg,
        };
        match check_patch(&patch2, &cwd) {
            PatchCheck::Applies => format_patch(&patch2, true),
            PatchCheck::Unchecked => format_patch(&patch2, false),
            PatchCheck::Rejected(why2) => format!(
                "AI Bridge: Codex's patch still does not apply cleanly ({why2}). \
                 Returning it for MANUAL review — DO NOT apply blind:\n\n```diff\n{patch2}\n```"
            ),
        }
    }

    /// The PRE-execution plan gate (Claude-called). Runs a Codex review round on
    /// the proposed plan over an isolated thread, records the verdict, and on
    /// APPROVE unlocks writes for the current task epoch. Multi-round: Claude
    /// revises and calls again until approved. Mirrors the Stop-gate's verdict
    /// machinery (same sentinel parser) but at the planning→execution boundary.
    fn plan_gate(&mut self, plan: &str) -> String {
        let cwd = std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| ".".to_string());
        // Capture the epoch BEFORE the (minutes-long) Codex call so `record` can
        // refuse to approve if a new task started meanwhile (TOCTOU guard).
        let epoch = crate::plan_gate::current_epoch(&cwd);
        // If this epoch is already approved but the plan materially changed, revoke
        // up front so writes re-block while the new plan is under review (the
        // minutes-long Codex call must not run with stale approval still open).
        crate::plan_gate::begin_review(&cwd, plan);
        // New task epoch → drop the prior plan dialogue so it can't anchor.
        if self.plan_epoch.as_deref() != Some(epoch.as_str()) {
            self.threads.remove(&TopicKey::PlanGate);
            self.plan_epoch = Some(epoch.clone());
        }
        let prompt = crate::plan_gate::prompt(plan);
        let review = match self.ask_topic(TopicKey::PlanGate, &prompt, &cwd) {
            Ok(r) if !r.trim().is_empty() => r,
            Ok(_) => return "AI Bridge: Codex returned an empty plan review.".to_string(),
            Err(e) => {
                return format!(
                    "AI Bridge: plan review unavailable (Codex error): {e}. \
                     Ask the user how to proceed (plan without review / retry / fix the issue)."
                )
            }
        };
        let verdict = gate::parse_verdict(&review);
        let findings = gate::findings(&review);
        match crate::plan_gate::record(&cwd, &epoch, plan, &verdict, &findings) {
            crate::plan_gate::Outcome::Approved => format!(
                "<AI-BRIDGE-APPROVE/> Codex APPROVED the plan — writes/Bash are now unlocked for \
                 this task. Proceed with execution.\n\n{findings}"
            ),
            crate::plan_gate::Outcome::Revise(f) => format!(
                "Codex REQUESTED CHANGES to the plan. Revise the plan to address these, then call \
                 `plan_gate` again (writes stay blocked until APPROVE):\n\n{f}"
            ),
            crate::plan_gate::Outcome::Stuck(f) => format!(
                "AI Bridge: the plan still has the same unresolved concerns after revision. \
                 STOP and ask the user how to proceed — do not keep retrying.\n\n{f}"
            ),
            crate::plan_gate::Outcome::NeedsInfo(f) => format!(
                "AI Bridge: Codex needs more information to judge the plan (or is blocked). \
                 Provide what it asks or check with the user, then call `plan_gate` again:\n\n{f}"
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
        let mut bundle = match crate::git::diff_bundle(cwd) {
            Ok(b) => b,
            Err(_) => return allow(), // not a git repo / git missing: nothing to gate
        };

        // Fold in work COMMITTED since the task's base so a `git commit` made BEFORE
        // this Stop can't hide it from review (closing the commit-bypass).
        // `committed_delta` degrades SAFELY on a diverged/missing base — it returns a
        // warned net/full-tree diff (embedded in the text), never a silent skip — so
        // here we just fold it in. No recorded base ⇒ uncommitted-only (pre-0.5.5
        // behavior, no regression; `doctor` flags the missing task-start hook).
        match crate::review_frontier::read(cwd, session).and_then(|f| {
            f.base_spec()
                .map(|base| crate::git::committed_delta(cwd, base))
        }) {
            Some(cd) => {
                if !cd.is_empty {
                    bundle = bundle.with_committed(&cd.text);
                }
            }
            None => log_gate(
                cwd,
                "no review base recorded — reviewing uncommitted tree only \
                 (re-run `aibridge init` to record a task-start base)",
            ),
        }

        if bundle.is_empty {
            // Nothing committed-since-base AND a clean tree ⇒ nothing to review.
            crate::review_frontier::set_status(
                cwd,
                session,
                crate::review_frontier::STATUS_APPROVED,
            );
            return allow();
        }
        log_gate(
            cwd,
            &format!("reviewing diff bundle: {} bytes", bundle.text.len()),
        );
        // Key the in-memory GateState by the REPO ROOT (not the raw cwd) so it matches
        // the repo-root keying the on-disk frontier/receipt use. Otherwise a Stop fired
        // from a subdir would split one repo's review state across two keys, drifting the
        // loop/block counters from the persisted allowed-diff receipt (Codex review).
        let key = format!(
            "{}::{session}",
            crate::git::repo_root(cwd).as_deref().unwrap_or(cwd)
        );
        let dh = bundle.hash;
        // Read the approved plan ONCE: it scopes the review prompt AND binds the
        // persisted approval receipt, so a CHANGED plan with an identical diff still
        // re-reviews (the receipt is keyed by this scope hash).
        let approved_plan = crate::plan_gate::approved_plan(cwd);
        let plan_hash = gate::hash_str(approved_plan.as_deref().unwrap_or(""));
        let mut state = self.gates.remove(&key).unwrap_or_default();
        // Hydrate the "already approved this exact diff" fast-path from disk so an MCP
        // reconnect (VS Code reload) doesn't force a redundant minutes-long re-review.
        // Honored only when the receipt's policy version + plan scope still match (else
        // None → review normally). Live in-memory state already wins, so we only fill a
        // freshly-defaulted slot.
        if state.last_allowed_diff_hash.is_none() {
            state.last_allowed_diff_hash =
                crate::review_frontier::read_allowed_hash(cwd, session, plan_hash);
        }
        let decision = self.gate_decide(&mut state, &bundle, cwd, approved_plan.as_deref());
        // Map the outcome to the frontier status so the NEXT task start won't advance
        // the base over unresolved debt. CRUCIAL: an allow is only a genuine APPROVE
        // when the gate recorded THIS diff as allowed; a fail-ask "delivery" allow
        // (so Claude can ask the user) does NOT set that, and must stay needs_user —
        // otherwise unresolved debt would be laundered into `approved` (Codex find).
        let status = if decision != "{}" {
            crate::review_frontier::STATUS_BLOCKED
        } else if state.last_allowed_diff_hash == Some(dh) {
            crate::review_frontier::STATUS_APPROVED
        } else {
            crate::review_frontier::STATUS_NEEDS_USER
        };
        self.gates.insert(key, state);
        // Persist the approval receipt so the fast-path survives a reconnect — bound to
        // the diff hash, the plan scope it was approved under, and the policy version.
        if status == crate::review_frontier::STATUS_APPROVED {
            crate::review_frontier::set_allowed_hash(cwd, session, dh, plan_hash);
        }
        crate::review_frontier::set_status(cwd, session, status);
        decision
    }

    fn gate_decide(
        &mut self,
        st: &mut gate::GateState,
        bundle: &crate::git::DiffBundle,
        cwd: &str,
        approved_plan: Option<&str>,
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

        // The pre-approved plan (read once by the caller) lets the reviewer flag changes
        // that fall outside the approved scope or high-risk actions the plan never named
        // — the soft-telemetry half of plan-gate v2 (we don't hard-fence files).
        let prompt = gate::prompt_with_scope(&bundle.text, approved_plan);
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

/// Normalize + validate a consult topic. A topic is REQUIRED (blank is rejected) —
/// there is no anonymous fallback channel: every consult is a stable, isolated,
/// persisted dialogue so context is never mixed across unrelated subjects. Enforces
/// a semantic kebab-case name (mirrors the predecessor's rules) so Claude can't
/// fragment or collide topics with vague/auto-generated ids.
fn normalize_topic(raw: &str) -> Result<String, String> {
    let t = raw.trim().to_lowercase();
    if t.is_empty() {
        return Err(
            "a 'topic' is required — pass a stable kebab-case name like repo-feature-phase \
             (each topic is an isolated, persisted dialogue)"
                .to_string(),
        );
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

/// Outcome of validating a proposed patch with `git apply --check`.
enum PatchCheck {
    /// Applies cleanly.
    Applies,
    /// Rejected, with the reason from git.
    Rejected(String),
    /// Could not validate (not a git repo / git missing) — returned as-is.
    Unchecked,
}

/// Implementer prompt: demand a single unified-diff patch in a strict envelope and
/// nothing else, so the output is machine-extractable and applyable.
fn implement_prompt(task: &str) -> String {
    format!(
        "You are an implementer. Produce a fix for the TASK as a SINGLE git unified \
         diff and NOTHING else outside the envelope.\n\n\
         Output EXACTLY this shape:\n\
         <AI-BRIDGE-PATCH>\n\
         diff --git a/path b/path\n\
         ...real hunks, paths relative to the repo root...\n\
         </AI-BRIDGE-PATCH>\n\
         then a final line that is EXACTLY one of:\n\
         <AI-BRIDGE-IMPLEMENTED/>  (a patch is provided)\n\
         <AI-BRIDGE-BLOCKED/>      (cannot do it safely — one-line reason after it)\n\
         <AI-BRIDGE-NEEDS-INFO/>   (missing context — one-line question after it)\n\n\
         Rules: no prose outside the envelope; only real `diff --git` hunks; never \
         invent contents of files you didn't inspect. If blocked or needing info, \
         emit an EMPTY <AI-BRIDGE-PATCH></AI-BRIDGE-PATCH> and the matching tag.\n\n\
         TASK:\n{task}"
    )
}

/// Extract the patch body from the `<AI-BRIDGE-PATCH>…</AI-BRIDGE-PATCH>` envelope,
/// stripping an optional ``` fence. None if the envelope is absent or empty.
fn extract_patch(raw: &str) -> Option<String> {
    const OPEN: &str = "<AI-BRIDGE-PATCH>";
    const CLOSE: &str = "</AI-BRIDGE-PATCH>";
    let start = raw.find(OPEN)? + OPEN.len();
    let rest = &raw[start..];
    let end = rest.find(CLOSE)?;
    let mut body = rest[..end].trim();
    if let Some(s) = body.strip_prefix("```diff") {
        body = s.trim();
    } else if let Some(s) = body.strip_prefix("```") {
        body = s.trim();
    }
    // Strip a CLOSING fence only when it's on its own line, so a stray ``` inside
    // trailing prose isn't mistaken for the fence (git apply would reject prose
    // anyway, but this avoids a confusing retry).
    if let Some(s) = body.strip_suffix("```") {
        if s.is_empty() || s.ends_with('\n') {
            body = s.trim();
        }
    }
    if body.is_empty() {
        return None;
    }
    // `git apply` requires the patch to end with a newline; the trims above
    // removed it, which makes git report "corrupt patch at line N". Restore one.
    let mut patch = body.to_string();
    if !patch.ends_with('\n') {
        patch.push('\n');
    }
    Some(patch)
}

/// Extract a patch from Codex's reply, or render a caller-facing message when
/// there is none — a BLOCKED/NEEDS-INFO decline, or a missing envelope. ONE
/// extraction point, shared by the first attempt AND the retry (so a decline on
/// either is surfaced cleanly, not mislabeled "no patch").
fn patch_or_message(raw: &str) -> Result<String, String> {
    if let Some(patch) = extract_patch(raw) {
        return Ok(patch);
    }
    if raw.contains("<AI-BRIDGE-BLOCKED/>") {
        Err(format!(
            "AI Bridge: Codex BLOCKED the implementation.\n\n{}",
            raw.trim()
        ))
    } else if raw.contains("<AI-BRIDGE-NEEDS-INFO/>") {
        Err(format!(
            "AI Bridge: Codex needs more info to implement this.\n\n{}",
            raw.trim()
        ))
    } else {
        Err(format!(
            "AI Bridge: Codex returned no patch envelope. Raw reply:\n\n{}",
            raw.trim()
        ))
    }
}

/// Validate a patch with `git apply --check` (via stdin, no temp file). Only runs
/// inside a git work tree; otherwise returns `Unchecked`.
fn check_patch(patch: &str, cwd: &str) -> PatchCheck {
    use std::process::Stdio;
    let git = match DefaultPlatform::find_executable("git") {
        Ok(g) => g,
        Err(_) => return PatchCheck::Unchecked,
    };
    let in_repo = DefaultPlatform::command_for(&git)
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(cwd)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !in_repo {
        return PatchCheck::Unchecked;
    }
    let mut child = match DefaultPlatform::command_for(&git)
        .args(["apply", "--check", "-"])
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return PatchCheck::Rejected(e.to_string()),
    };
    if let Some(mut sin) = child.stdin.take() {
        let _ = sin.write_all(patch.as_bytes());
        if !patch.ends_with('\n') {
            let _ = sin.write_all(b"\n"); // git apply needs a trailing newline
        }
        // `sin` dropped here → stdin closed so git can finish.
    }
    match child.wait_with_output() {
        Ok(o) if o.status.success() => PatchCheck::Applies,
        Ok(o) => PatchCheck::Rejected(String::from_utf8_lossy(&o.stderr).trim().to_string()),
        Err(e) => PatchCheck::Rejected(e.to_string()),
    }
}

/// Wrap a proposed patch with a clear banner for Claude, stating whether it was
/// validated (`git apply --check`) or validation was skipped (not a git repo).
fn format_patch(patch: &str, validated: bool) -> String {
    let v = if validated {
        "validated with `git apply --check`"
    } else {
        "validation SKIPPED (not in a git repo / git not found)"
    };
    format!(
        "AI Bridge implementer — PROPOSED patch ({v}; UNTESTED: a read-only peer can't run it — \
         review and run tests after applying):\n\n```diff\n{patch}\n```"
    )
}

/// Hard wall-clock cap for a `run` command (tests/builds finish well within this;
/// a hung command is killed rather than blocking forever).
const RUN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);
/// Per-stream output cap (bytes) so a noisy command can't flood the context or
/// buffer unbounded; the rest is read-and-discarded with a truncation marker.
const RUN_OUTPUT_CAP: usize = 16_000;

/// Runner: execute a shell command in the project dir and return a STRUCTURED
/// result (exit code, duration, capped stdout/stderr, timeout marker). It is only
/// reachable by the MCP client (Claude — which already has Bash, so this adds no
/// new privilege), never by the read-only Codex peer. stdin is null; output is
/// drained on threads (no pipe-full deadlock) and the child is killed on timeout.
fn run_command(command: &str) -> String {
    use std::io::Read;
    use std::process::{Command, Stdio};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| ".".to_string());

    #[cfg(windows)]
    let mut cmd = {
        let mut c = Command::new("cmd");
        use std::os::windows::process::CommandExt;
        // `raw_arg`, NOT `arg`: `Command::arg` applies MSVCRT quoting (wraps in
        // `"..."` and escapes embedded quotes as `\"`), which `cmd.exe` does not
        // understand — so a command containing a quoted path with spaces (e.g.
        // `node --check "d:\Cursor Projects\…"`) gets split at the first space.
        //
        // `/D /S /C "<command>"` is the robust form: `/D` skips AutoRun registry
        // hooks; `/S` + our own outer quote pair makes cmd strip ONLY that outer
        // pair and pass the command through verbatim — so it survives even when the
        // command itself starts with a quoted exe path AND has quoted args (e.g.
        // `"C:\Program Files\nodejs\node.exe" "C:\a b\x.js"`), which a bare `/C`
        // would mangle via cmd's first/last-quote stripping rule.
        c.raw_arg("/D")
            .raw_arg("/S")
            .raw_arg("/C")
            .raw_arg(format!("\"{command}\""));
        c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        c
    };
    #[cfg(not(windows))]
    let mut cmd = {
        let mut c = Command::new("sh");
        c.arg("-c").arg(command);
        // Own process group so a timeout kills the whole tree (the shell's children
        // — the real `cargo test`/`npm`/etc.), not just the `sh` wrapper.
        use std::os::unix::process::CommandExt;
        c.process_group(0);
        c
    };
    cmd.current_dir(&cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let start = Instant::now();
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return format!("AI Bridge run — failed to start `{command}`: {e}"),
    };
    let pid = child.id();

    // Drain both pipes on threads so output never deadlocks the wait, AND cap while
    // reading: take up to RUN_OUTPUT_CAP bytes, then discard the rest (so we never
    // buffer unbounded output, and the child isn't backpressured into hanging).
    // Returns (capped_lossy_text, was_truncated).
    let drain = |pipe: Option<Box<dyn Read + Send>>| {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let mut truncated = false;
            if let Some(mut p) = pipe {
                let _ = p.by_ref().take(RUN_OUTPUT_CAP as u64).read_to_end(&mut buf);
                let extra = std::io::copy(&mut p, &mut std::io::sink()).unwrap_or(0);
                truncated = extra > 0;
            }
            let _ = tx.send((String::from_utf8_lossy(&buf).into_owned(), truncated));
        });
        rx
    };
    let orx = drain(
        child
            .stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let erx = drain(
        child
            .stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );

    let deadline = start + RUN_TIMEOUT;
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break Some(st),
            Ok(None) => {
                if Instant::now() >= deadline {
                    kill_tree(pid); // kill the whole tree, not just the shell wrapper
                    let _ = child.kill(); // belt-and-suspenders on the direct child
                    let _ = child.wait();
                    timed_out = true;
                    break None;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => break None,
        }
    };
    let dur = start.elapsed();
    // Bound the wait for the drain threads: if a DETACHED/daemonized descendant
    // escaped the tree/group kill above and still holds the pipe open, the capped
    // read never hits EOF — don't freeze the server. We accept a leaked drain
    // thread + partial output in that rare case (per stream, so ≤10s total).
    let grace = Duration::from_secs(5);
    let (mut stdout, otrunc) = orx.recv_timeout(grace).unwrap_or_default();
    let (mut stderr, etrunc) = erx.recv_timeout(grace).unwrap_or_default();
    if otrunc {
        stdout.push_str("\n…[output truncated]");
    }
    if etrunc {
        stderr.push_str("\n…[output truncated]");
    }
    let head = if timed_out {
        format!(
            "AI Bridge run — TIMED OUT after {}s (process tree killed)",
            RUN_TIMEOUT.as_secs()
        )
    } else {
        let exit = status
            .and_then(|s| s.code())
            .map(|c| c.to_string())
            .unwrap_or_else(|| "terminated-by-signal".to_string());
        format!("AI Bridge run — exit {exit} in {:.1}s", dur.as_secs_f64())
    };
    format!("{head}\ncmd: {command}\ncwd: {cwd}\n\n--- stdout ---\n{stdout}\n\n--- stderr ---\n{stderr}")
}

/// Kill an entire process tree by root pid — on timeout the shell's children (the
/// real test/build process) must die too, not just the `cmd /C` / `sh -c` wrapper.
fn kill_tree(pid: u32) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let _ = std::process::Command::new("taskkill")
            .args(["/F", "/T", "/PID", &pid.to_string()])
            .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    #[cfg(not(windows))]
    {
        // The child leads its own process group (process_group(0)), so the group id
        // equals its pid; a negative pid signals the whole group.
        let _ = std::process::Command::new("kill")
            .args(["-KILL", &format!("-{pid}")])
            .status();
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
            if let Ok((gate_tid, _)) =
                peer.open_thread(GATE_PRIMER, &cwd, crate::codex::REVIEW_REASONING_EFFORT)
            {
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
                              // Activate a staged plan gate now that THIS server (which provides `plan_gate`)
                              // is up — closes the post-`init` deadlock where the gate would block before the
                              // approval tool was reachable.
    if let Ok(cwd) = std::env::current_dir() {
        crate::plan_gate::promote_pending(&cwd.display().to_string());
    }
    server.spawn_warming_if_absent(); // warm Codex in the background while the user works
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
             tradeoff. Does not gate final output. A stable `topic` is REQUIRED: each topic is a \
             continuous, isolated, persisted dialogue — reuse the SAME topic for every follow-up \
             on one subject so Codex keeps context (there is no anonymous channel).",
            json!({
                "type": "object",
                "properties": {
                    "question": { "type": "string", "description": "What to ask the Codex peer." },
                    "topic": { "type": "string", "description": "REQUIRED. Stable kebab-case dialogue topic for this subject (e.g. 'repo-feature-phase'). Reuse the same topic for every follow-up so Codex keeps context across calls and sessions." },
                    "reset": { "type": "boolean", "description": "Start this topic fresh, discarding prior turns." }
                },
                "required": ["question", "topic"]
            })
        ),
        tool_with(
            "implement",
            "Ask AI Bridge/Codex to IMPLEMENT a focused task as a proposed unified-diff \
             patch (validated with `git apply --check` when in a git repo). Returns the \
             patch for you to review + apply; it is UNTESTED. Use for a second-model \
             implementation or when you want Codex to draft a fix.",
            json!({
                "type": "object",
                "properties": {
                    "task": { "type": "string", "description": "What to implement (be specific; reference files/symbols)." }
                },
                "required": ["task"]
            })
        ),
        tool_with(
            "run",
            "Run a shell command in the project dir and return a STRUCTURED result \
             (exit code, duration, capped stdout/stderr, timeout marker). Useful for \
             tests/builds when you want a bounded, captured result.",
            json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "The shell command to run (e.g. 'cargo test', 'npm test')." }
                },
                "required": ["command"]
            })
        ),
        tool_with(
            "plan_gate",
            "PRE-execution plan review: before you start CODING a task, submit your todolist/plan \
             here for a Codex second opinion. Returns a verdict — APPROVE, REQUEST_CHANGES \
             (revise and call again), or needs-info. When the plan gate is active (default-on), \
             writes (Write/Edit/MultiEdit/NotebookEdit) and Bash are BLOCKED until this returns \
             <AI-BRIDGE-APPROVE/> for the current task. Do read-only discovery (Read/Grep/Glob) \
             first, then submit a concrete plan.",
            json!({
                "type": "object",
                "properties": {
                    "plan": { "type": "string", "description": "The structured plan to review: todos, approach, intended_files, risk_surfaces, and test_plan. Be concrete." }
                },
                "required": ["plan"]
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
    fn blank_topic_rejected() {
        // A topic is mandatory now — there is no anonymous/scratch fallback.
        assert!(normalize_topic("").is_err());
        assert!(normalize_topic("   ").is_err());
    }

    // Guards the Windows `run` bug: a command containing a quoted path with spaces
    // must survive `cmd /D /S /C` quoting (it was mangled by the old `arg` form).
    #[cfg(windows)]
    #[test]
    fn run_command_handles_quoted_paths_with_spaces() {
        use std::io::Write as _;
        let dir = std::env::temp_dir().join(format!("ai bridge run test {}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("hello file.txt");
        std::fs::File::create(&file)
            .unwrap()
            .write_all(b"QUOTED_SPACE_OK\n")
            .unwrap();
        // `type` is a cmd builtin; the absolute path has spaces in both the dir and
        // the filename, so it only succeeds if the quotes reach cmd intact.
        let out = run_command(&format!("type \"{}\"", file.display()));
        std::fs::remove_dir_all(&dir).ok();
        assert!(out.contains("exit 0"), "expected exit 0, got:\n{out}");
        assert!(
            out.contains("QUOTED_SPACE_OK"),
            "quoted space-path was mangled; output:\n{out}"
        );
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
        assert!(normalize_topic("scratch").is_err()); // too vague a name
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
    fn extract_patch_pulls_envelope_and_strips_fences() {
        let raw = "prose\n<AI-BRIDGE-PATCH>\ndiff --git a/x b/x\n+line\n</AI-BRIDGE-PATCH>\n<AI-BRIDGE-IMPLEMENTED/>";
        let p = extract_patch(raw).unwrap();
        assert!(p.contains("diff --git a/x b/x") && p.contains("+line"));
        assert!(!p.contains("AI-BRIDGE"), "envelope tags must be stripped");
        assert!(p.ends_with('\n'), "git apply requires a trailing newline");

        let fenced = "<AI-BRIDGE-PATCH>\n```diff\ndiff --git a/y b/y\n```\n</AI-BRIDGE-PATCH>";
        assert!(extract_patch(fenced).unwrap().starts_with("diff --git a/y"));
        assert!(!extract_patch(fenced).unwrap().contains("```"));

        assert_eq!(extract_patch("<AI-BRIDGE-PATCH></AI-BRIDGE-PATCH>"), None); // empty body
        assert_eq!(extract_patch("no envelope at all"), None);
    }

    #[test]
    fn patch_or_message_extracts_or_explains() {
        // A real patch present → Ok(patch).
        assert!(patch_or_message(
            "<AI-BRIDGE-PATCH>\ndiff --git a/z b/z\n</AI-BRIDGE-PATCH>\n<AI-BRIDGE-IMPLEMENTED/>"
        )
        .is_ok());
        // Declines (empty envelope + tag) → Err with a labeled message. This is the
        // path that must ALSO work on the retry response.
        let blocked =
            patch_or_message("<AI-BRIDGE-PATCH></AI-BRIDGE-PATCH>\n<AI-BRIDGE-BLOCKED/> nope");
        assert!(blocked.is_err() && blocked.unwrap_err().contains("BLOCKED"));
        let needs = patch_or_message(
            "<AI-BRIDGE-PATCH></AI-BRIDGE-PATCH>\n<AI-BRIDGE-NEEDS-INFO/> which file?",
        );
        assert!(needs.is_err() && needs.unwrap_err().contains("more info"));
        // No envelope at all → Err.
        assert!(patch_or_message("just prose, no tags").is_err());
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
