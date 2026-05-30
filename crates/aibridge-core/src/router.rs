//! v0.32 Phase 1 — SHADOW-MODE execution router (LOG-ONLY, default OFF).
//!
//! Computes a deterministic `single` vs `orchestrated` recommendation (+ a fan-out degree)
//! from the submitted plan text and the repo's file shape. In Phase 1 this is **telemetry
//! only**: it has ZERO authority over plan approval, execution, workflow launch, or the Stop
//! gate. The point is to collect recommendation-vs-actual-outcome data so the router can be
//! validated against real Stop results BEFORE it is ever promoted to advisory/active.
//!
//! Design + guardrails: Codex consult `aibridge-v032-router-phase1` (APPROVE). The analyzer
//! ([`analyze`]) is PURE (no IO) so it is exhaustively unit-testable; the only IO is the
//! best-effort append in [`log_recommendation`], which can never block or change behavior.

use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// The execution strategy the router would suggest (telemetry only in Phase 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// One coherent agent context (the safe default — sequential/deep/coupled work).
    Single,
    /// Parallelizable fan-out across independent units (broad work).
    Orchestrated,
}

impl Route {
    fn as_str(self) -> &'static str {
        match self {
            Route::Single => "single",
            Route::Orchestrated => "orchestrated",
        }
    }
}

/// Cheap, deterministic signals derived from the plan + repo (no LLM, no IO).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RouterSignals {
    pub plan_chars: usize,
    pub bullet_count: usize,
    pub risk_classes: Vec<String>,
    pub mentioned_paths: usize,
    pub repo_area_count: usize,
    pub shared_core_files: usize,
    pub serial_markers: usize,
}

/// A telemetry recommendation. `fanout` is 1 for `Single`. `confidence` is 0..=100 and is
/// deliberately LOW for `Orchestrated` (Phase 1 leans heavily toward the safe `Single`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouterRecommendation {
    pub route: Route,
    pub fanout: u32,
    pub confidence: u8,
    pub reasons: Vec<String>,
    pub signals: RouterSignals,
}

impl RouterRecommendation {
    /// JSON for the telemetry log (no IO).
    pub fn to_value(&self) -> Value {
        json!({
            "route": self.route.as_str(),
            "fanout": self.fanout,
            "confidence": self.confidence,
            "reasons": self.reasons,
            "signals": {
                "plan_chars": self.signals.plan_chars,
                "bullet_count": self.signals.bullet_count,
                "risk_classes": self.signals.risk_classes,
                "mentioned_paths": self.signals.mentioned_paths,
                "repo_area_count": self.signals.repo_area_count,
                "shared_core_files": self.signals.shared_core_files,
                "serial_markers": self.signals.serial_markers,
            },
        })
    }
}

/// Sequential-dependency words in a plan (lowercased, word-boundary matched). A plan with
/// several of these is ordered work → keep it `Single`.
const SERIAL_MARKERS: &[&str] = &[
    "then",
    "after",
    "before",
    "first",
    "once",
    "finally",
    "subsequently",
    "afterwards",
];

/// Basenames that signal a shared/core file (editing these in parallel courts merge/ordering
/// hazards), plus path fragments for the gate's own safety-core.
const SHARED_CORE_BASENAMES: &[&str] = &["cargo.toml", "cargo.lock", "lib.rs", "main.rs", "mod.rs"];
const SHARED_CORE_FRAGMENTS: &[&str] = &[
    "plan_gate",
    "plan_receipt",
    "review_frontier",
    "/mcp.rs",
    "migration",
    "schema",
];

/// True when `tok` looks like a file path worth matching (has a `/` or a dotted extension).
fn is_path_like(tok: &str) -> bool {
    tok.contains('/') || (tok.contains('.') && !tok.starts_with('.') && !tok.ends_with('.'))
}

/// The coarse "area" of a repo path: `crates/<name>` keeps the crate; otherwise the first
/// path component. Used to count DISTINCT areas a plan touches.
fn area_of(path: &str) -> String {
    let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    if parts.first() == Some(&"crates") && parts.len() >= 2 {
        format!("crates/{}", parts[1])
    } else {
        parts.first().map(|s| s.to_string()).unwrap_or_default()
    }
}

/// PURE deterministic analysis (no IO). `repo_files` is the repo's tracked files (relative,
/// `/`-separated, e.g. from `git ls-files`); `max_fanout` caps an `Orchestrated` degree.
pub fn analyze(plan: &str, repo_files: &[String], max_fanout: u32) -> RouterRecommendation {
    let lower = plan.to_lowercase();
    let mut signals = RouterSignals {
        plan_chars: plan.chars().count(),
        ..Default::default()
    };

    // Bullets: lines beginning with -, *, or "N." after trimming.
    signals.bullet_count = plan
        .lines()
        .filter(|l| {
            let t = l.trim_start();
            t.starts_with("- ")
                || t.starts_with("* ")
                || t.starts_with("• ")
                || t.chars().next().is_some_and(|c| c.is_ascii_digit())
                    && t.trim_start_matches(|c: char| c.is_ascii_digit())
                        .starts_with(['.', ')'])
        })
        .count();

    // High-risk class (reuse the gate's scanner over the plan prose).
    if let Some(class) = crate::plan_gate::high_risk_class(plan) {
        signals.risk_classes.push(class.to_string());
    }

    // Serial markers (token-based so "thenable" / a path don't false-match).
    let tokens: Vec<String> = lower
        .split(|c: char| !c.is_alphanumeric() && c != '/' && c != '.' && c != '_' && c != '-')
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect();
    signals.serial_markers = tokens
        .iter()
        .filter(|t| SERIAL_MARKERS.contains(&t.as_str()))
        .count();

    // Mentioned repo files: match path-like tokens against the tracked file list (exact path
    // or basename suffix). Collect the matched files (deduped), then derive areas + core hits.
    let mut matched: Vec<&String> = Vec::new();
    for tok in plan
        .split(|c: char| c.is_whitespace() || "`'\"(),;:".contains(c))
        .map(|t| t.trim_matches(|c: char| "`'\".,!?()[]".contains(c)))
        .filter(|t| !t.is_empty() && is_path_like(t))
    {
        // Normalize Windows-style separators so a plan that writes `crates\x\y.rs` matches the
        // `/`-separated tracked path.
        let tl = tok.replace('\\', "/").to_lowercase();
        if let Some(f) = repo_files.iter().find(|f| {
            let fl = f.to_lowercase();
            fl == tl || fl.ends_with(&format!("/{tl}")) || fl == tl.trim_start_matches("./")
        }) {
            if !matched.contains(&f) {
                matched.push(f);
            }
        }
    }
    signals.mentioned_paths = matched.len();

    let mut areas: Vec<String> = Vec::new();
    for f in &matched {
        let fl = f.to_lowercase();
        let base = fl.rsplit('/').next().unwrap_or(&fl);
        if SHARED_CORE_BASENAMES.contains(&base) || SHARED_CORE_FRAGMENTS.iter().any(|frag| fl.contains(frag)) {
            signals.shared_core_files += 1;
        }
        let a = area_of(f);
        if !a.is_empty() && !areas.contains(&a) {
            areas.push(a);
        }
    }
    signals.repo_area_count = areas.len();

    // ---- Decision: default SINGLE; orchestrate ONLY for clearly-separated, low-risk breadth.
    let single = |confidence: u8, reason: &str| RouterRecommendation {
        route: Route::Single,
        fanout: 1,
        confidence,
        reasons: vec![reason.to_string()],
        signals: signals.clone(),
    };

    if !signals.risk_classes.is_empty() {
        return single(85, "high-risk command class present — keep one coherent context");
    }
    if signals.serial_markers >= 2 {
        return single(75, "sequential dependency markers — ordered work");
    }
    if signals.shared_core_files > 0 {
        return single(70, "touches shared/core files — parallel edits would conflict");
    }
    if signals.repo_area_count >= 3 && signals.mentioned_paths >= 4 && max_fanout >= 2 {
        let fanout = (signals.repo_area_count as u32).min(max_fanout).max(2);
        return RouterRecommendation {
            route: Route::Orchestrated,
            fanout,
            confidence: 50, // deliberately low — Phase 1 telemetry, not policy
            reasons: vec![format!(
                "{} independent repo areas across {} files, no high-risk/serial/shared-core signal",
                signals.repo_area_count, signals.mentioned_paths
            )],
            signals: signals.clone(),
        };
    }
    single(80, "default — no clear independent fan-out detected")
}

/// Append one JSONL telemetry line to `path`, best-effort. Returns whether it was written.
/// Pure-ish IO (any path) so failure handling is testable.
fn append_jsonl(path: &Path, line: &str) -> bool {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return false;
        }
    }
    match std::fs::OpenOptions::new().create(true).append(true).open(path) {
        Ok(mut f) => writeln!(f, "{line}").is_ok(),
        Err(_) => false,
    }
}

/// Tracked repo files (relative, `/`-separated) via `git ls-files`, best-effort (empty on any
/// failure). Used only to feed the PURE [`analyze`] — the router never mutates the repo.
pub fn repo_files(cwd: &str) -> Vec<String> {
    match std::process::Command::new("git")
        .args(["ls-files"])
        .current_dir(cwd)
        .output()
    {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .lines()
            .map(|l| l.trim().replace('\\', "/"))
            .filter(|l| !l.is_empty())
            .collect(),
        _ => Vec::new(),
    }
}

/// The shadow telemetry log path (`<dir>/.ai-bridge/router.jsonl`). Walks up for an EXISTING
/// `.ai-bridge` dir using ONLY the filesystem — no `git` shell-out — so it is safe to call on
/// the Stop hook's critical path (Codex review). `None` when no `.ai-bridge` ancestor exists
/// (so it never creates `.ai-bridge/` in a repo that isn't using the gate).
fn router_log_path(cwd: &str) -> Option<PathBuf> {
    let mut p: &Path = Path::new(cwd);
    loop {
        let candidate = p.join(".ai-bridge");
        if candidate.is_dir() {
            return Some(candidate.join("router.jsonl"));
        }
        p = p.parent()?;
    }
}

/// Reserved top-level keys the telemetry join relies on — caller `fields` may not overwrite them.
const RESERVED_EVENT_KEYS: &[&str] = &["event", "schema_version", "ts_ms", "epoch", "plan_hash"];

/// Best-effort: append a generic telemetry event to the shadow log. Keyed by `epoch` (+ an
/// optional `plan_hash`) so the recommendation and the later outcomes (plan_gate_outcome /
/// stop_outcome) join on the same task. `fields` (if an object) is merged in. NEVER blocks /
/// NEVER errors to the caller (returns false on any failure). Caller invokes only in shadow mode.
pub fn log_event(
    cwd: &str,
    ts_ms: u128,
    event: &str,
    epoch: &str,
    plan_hash: Option<&str>,
    fields: Value,
) -> bool {
    let Some(path) = router_log_path(cwd) else {
        return false;
    };
    let mut obj = json!({
        "event": event,
        "schema_version": 1,
        "ts_ms": ts_ms as u64,
        "epoch": epoch,
        "plan_hash": plan_hash,
    });
    if let (Some(o), Some(extra)) = (obj.as_object_mut(), fields.as_object()) {
        for (k, v) in extra {
            // Never let caller fields clobber the reserved join keys (epoch/plan_hash/...).
            if !RESERVED_EVENT_KEYS.contains(&k.as_str()) {
                o.insert(k.clone(), v.clone());
            }
        }
    }
    append_jsonl(&path, &obj.to_string())
}

/// Best-effort: append a `router_recommendation` telemetry event (thin wrapper over [`log_event`]).
pub fn log_recommendation(
    cwd: &str,
    ts_ms: u128,
    epoch: &str,
    session: Option<&str>,
    plan_hash: &str,
    head_oid: Option<&str>,
    rec: &RouterRecommendation,
) -> bool {
    log_event(
        cwd,
        ts_ms,
        "router_recommendation",
        epoch,
        Some(plan_hash),
        json!({ "session": session, "head_oid": head_oid, "recommendation": rec.to_value() }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files() -> Vec<String> {
        [
            "crates/aibridge-core/src/plan_gate.rs",
            "crates/aibridge-core/src/router.rs",
            "crates/aibridge-core/src/health.rs",
            "crates/aibridge-platform/src/lib.rs",
            "crates/aibridge/src/tui.rs",
            "crates/aibridge/src/main.rs",
            "docs/macos-cli-detection.md",
            "Cargo.toml",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect()
    }

    #[test]
    fn high_risk_plan_is_single() {
        let r = analyze("Refactor the deploy step and run `git push origin main`.", &files(), 8);
        assert_eq!(r.route, Route::Single);
        assert!(!r.signals.risk_classes.is_empty());
        assert_eq!(r.fanout, 1);
    }

    #[test]
    fn sequential_plan_is_single() {
        let r = analyze(
            "First update the schema, then run the migration, and after that edit the handlers.",
            &files(),
            8,
        );
        assert_eq!(r.route, Route::Single);
        assert!(r.signals.serial_markers >= 2);
    }

    #[test]
    fn shared_core_file_is_single() {
        // Touches a crate's lib.rs + a core gate file → parallel edits would conflict.
        let r = analyze(
            "Edit crates/aibridge-platform/src/lib.rs and crates/aibridge-core/src/plan_gate.rs.",
            &files(),
            8,
        );
        assert_eq!(r.route, Route::Single);
        assert!(r.signals.shared_core_files >= 1);
    }

    #[test]
    fn separated_areas_are_orchestrated_low_confidence() {
        // Three distinct, non-core files across three areas, no risk/serial/shared-core.
        let r = analyze(
            "Audit crates/aibridge-core/src/router.rs and crates/aibridge-core/src/health.rs \
             and crates/aibridge/src/tui.rs and docs/macos-cli-detection.md for typos.",
            &files(),
            8,
        );
        assert_eq!(r.route, Route::Orchestrated);
        assert!(r.fanout >= 2 && r.fanout <= 8);
        assert!(r.confidence <= 60, "orchestrated must be low-confidence in Phase 1");
        assert!(r.signals.mentioned_paths >= 4);
    }

    #[test]
    fn short_plan_defaults_to_single() {
        let r = analyze("Fix the typo in the README.", &files(), 8);
        assert_eq!(r.route, Route::Single);
        assert_eq!(r.confidence, 80);
    }

    #[test]
    fn max_fanout_zero_or_one_never_orchestrates() {
        let plan = "Audit crates/aibridge-core/src/router.rs and crates/aibridge-core/src/health.rs \
             and crates/aibridge/src/tui.rs and docs/macos-cli-detection.md.";
        assert_eq!(analyze(plan, &files(), 1).route, Route::Single);
        assert_eq!(analyze(plan, &files(), 0).route, Route::Single);
    }

    #[test]
    fn append_jsonl_writes_then_fails_safely() {
        let dir = std::env::temp_dir().join(format!("aibridge-router-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("router.jsonl");
        assert!(append_jsonl(&path, "{\"a\":1}"));
        assert!(append_jsonl(&path, "{\"a\":2}"));
        let body = std::fs::read_to_string(&path).unwrap();
        assert_eq!(body.lines().count(), 2, "appends, not overwrites");
        // A path whose parent is a FILE cannot be created → swallowed (false, no panic).
        let bad = path.join("nested.jsonl");
        assert!(!append_jsonl(&bad, "{}"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn recommendation_serializes_with_all_signals() {
        let r = analyze("Fix the typo.", &files(), 4);
        let v = r.to_value();
        assert_eq!(v["route"], "single");
        assert!(v["signals"].get("repo_area_count").is_some());
        assert!(v["reasons"].is_array());
    }

    #[test]
    fn windows_style_path_tokens_match_tracked_files() {
        // A plan written with `\` separators must still match the `/`-separated tracked path.
        let r = analyze(
            "Edit crates\\aibridge-core\\src\\health.rs and crates\\aibridge\\src\\tui.rs.",
            &files(),
            8,
        );
        assert!(
            r.signals.mentioned_paths >= 2,
            "windows-path tokens should match: {:?}",
            r.signals
        );
    }

    #[test]
    fn log_recommendation_with_no_aibridge_ancestor_is_a_noop() {
        // No `.ai-bridge` ancestor → router_log_path None → log returns false, nothing written
        // (and crucially never CREATES `.ai-bridge/` in a repo that isn't using the gate).
        let r = analyze("x", &[], 4);
        let fake = "/nonexistent-aibridge-router-xyzzy/sub/dir";
        assert!(!log_recommendation(fake, 1, "epoch", None, "hash", None, &r));
    }

    #[test]
    fn log_event_protects_reserved_keys_and_merges_extra() {
        // A temp dir containing `.ai-bridge/` (no git repo needed — router_log_path is fs-walk).
        let dir = std::env::temp_dir().join(format!("aibridge-router-ev-{}", std::process::id()));
        let _ = std::fs::create_dir_all(dir.join(".ai-bridge"));
        // `fields` tries to clobber the reserved `epoch` AND adds a legit `custom` key.
        assert!(log_event(
            dir.to_str().unwrap(),
            7,
            "plan_gate_outcome",
            "REAL-EPOCH",
            Some("REAL-HASH"),
            json!({ "epoch": "HACKED", "custom": 42 }),
        ));
        let body = std::fs::read_to_string(dir.join(".ai-bridge").join("router.jsonl")).unwrap();
        let v: Value = serde_json::from_str(body.lines().next_back().unwrap()).unwrap();
        assert_eq!(v["epoch"], "REAL-EPOCH", "reserved key must NOT be clobbered");
        assert_eq!(v["plan_hash"], "REAL-HASH");
        assert_eq!(v["event"], "plan_gate_outcome");
        assert_eq!(v["custom"], 42, "non-reserved field merges through");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
