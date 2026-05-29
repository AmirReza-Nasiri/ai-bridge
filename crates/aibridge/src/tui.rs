//! `aibridge status` (the dashboard; `tui` is a hidden alias) — one interactive
//! terminal screen consolidating what you'd otherwise run separately: `doctor`
//! (Health), live review status (Review), `review-mcp` (toggle which codex MCP servers
//! run during reviews), and self-`update`. ratatui + crossterm. The TUI is a SEPARATE
//! process from the MCP server, so it only READS the engine's status files (no
//! shared-state races); toggling writes review-mcp.json; the actual self-update runs
//! AFTER the TUI exits (clean terminal). `status --watch`/`--plain`/no-TTY → text.
//!
//! Design peer-reviewed (Codex): structured Review model from `read_status` (+ the
//! `status_report` verdict line); explicit `tui` command only (no bare-launch) and
//! it refuses without a TTY; ratatui's panic hook + `restore()` keep the terminal
//! from being left in raw mode; ASCII-only glyphs for legacy Windows consoles; the
//! App state model is kept separate from rendering/event-loop so navigation is unit-
//! tested without touching the real terminal or `~/.ai-bridge`.

use anyhow::Result;
use ratatui::{
    crossterm::event::{self, Event, KeyCode, KeyEventKind},
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph, Tabs, Wrap},
    DefaultTerminal, Frame,
};
use std::io::IsTerminal;
use std::time::{Duration, Instant};

use aibridge_core::doctor::{self, Check, Status};
use aibridge_core::{claude_mcp, codex_models, progress, review_mcp, skills};

/// Format a managed-skills `OpResult` into a single footer line. When `ok == false`, the
/// loud `⚠ NOT fully …` line is preferred so a partial failure is never hidden behind a
/// terser "kept …" / "removed …" line that happens to come earlier in the message.
fn format_managed_message(action: &str, r: &aibridge_core::managed_skills::OpResult) -> String {
    let pick = |needles: &[&str]| -> Option<String> {
        r.message
            .lines()
            .find(|l| needles.iter().any(|n| l.contains(n)))
            .map(|l| l.trim().to_string())
    };
    if !r.ok {
        let warn = pick(&["⚠", "NOT fully", "! "]).or_else(|| pick(&["kept", "removed"]));
        let key = warn.unwrap_or_else(|| format!("{action}: failed"));
        format!("{action} (PARTIAL) — {key}")
    } else {
        let key = pick(&["removed", "kept", "installed", "wrote", "ADDED"])
            .unwrap_or_else(|| format!("{action}: done"));
        format!("{action} — {key}")
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Health,
    Review,
    ClaudeMcpInspector,
    Mcp,
    Skills,
    Update,
    Debug,
}

impl Tab {
    /// Tab order: Claude MCP Inspector comes BEFORE Codex MCP per the user's request
    /// (mac dogfood, 2026-05-26). The Inspector is view-only in v1; safe management
    /// (config edits + restart UX) is deliberately a follow-up plan.
    const ALL: [Tab; 7] = [
        Tab::Health,
        Tab::Review,
        Tab::ClaudeMcpInspector,
        Tab::Mcp,
        Tab::Skills,
        Tab::Update,
        Tab::Debug,
    ];
    fn title(self) -> &'static str {
        match self {
            Tab::Health => "Health",
            Tab::Review => "Review",
            Tab::ClaudeMcpInspector => "Claude MCP Inspector",
            Tab::Mcp => "Codex MCP",
            Tab::Skills => "Skills",
            Tab::Update => "Update",
            Tab::Debug => "Debug",
        }
    }
    fn index(self) -> usize {
        Self::ALL.iter().position(|&t| t == self).unwrap_or(0)
    }
}

/// One codex MCP server's review-policy row.
struct McpRow {
    name: String,
    enabled: bool,
    interactive: bool,
}

/// The Codex-MCP tab has two levels: the server list, and (after Enter on a server)
/// that server's per-tool view.
#[derive(Clone, Copy, PartialEq, Eq)]
enum McpView {
    Servers,
    Tools,
}

// ───────────────────────── v0.29 (O1c): review-model selector ─────────────────────────
//
// The Review tab is an interactive picker for AI Bridge's review MODEL: a "Default"
// row (use codex's config.toml — no override), one row per live model from codex's
// own cache (`codex_models::list_models`), and a "Custom" row for a free-text slug
// (also the fallback when the cache is missing). Selecting persists via
// `review_mcp::set_codex_model`. The TUI is a SEPARATE process from the MCP server, and
// O1b pins ONE review model for the whole server lifetime — so a change is only
// "configured" on disk and takes effect after Claude Code / the AI Bridge MCP server is
// restarted (NOT on the next review-child spawn). The TUI never renders a model as
// "active" and never edits the context-window override (CLI-only; preserved on save).

/// One selectable row in the review-model picker.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ModelChoice {
    /// Clear the override → codex uses its own config.toml default (clears ctx too).
    Default,
    /// A concrete model from codex's live cache.
    Model(codex_models::ModelInfo),
    /// Free-text slug entry (also the fallback when the live cache is empty).
    Custom,
}

/// Outcome of feeding one key to the custom-slug text editor.
#[derive(Debug, Clone, PartialEq, Eq)]
enum InputOutcome {
    /// Buffer changed (or the key was ignored) — stay in input mode.
    Editing,
    /// Enter pressed — submit the trimmed buffer.
    Submit(String),
    /// Esc pressed — leave input mode without saving.
    Cancel,
}

/// Pure: the picker rows = [Default, one per cached model, Custom]. Custom is always
/// last so it is the fallback even when `rows` is empty (→ [Default, Custom]).
fn model_choices(rows: &[codex_models::ModelInfo]) -> Vec<ModelChoice> {
    let mut out = Vec::with_capacity(rows.len() + 2);
    out.push(ModelChoice::Default);
    out.extend(rows.iter().cloned().map(ModelChoice::Model));
    out.push(ModelChoice::Custom);
    out
}

/// Pure: index of the row the ON-DISK config points to (the "configured" row — NOT
/// "active": the running server may still hold a different pinned model). No model → 0
/// (Default); a configured model present in the cache → its row; a configured model NOT
/// in the cache → the Custom row (last). ctx is irrelevant to which model is configured.
fn configured_choice_index(
    rows: &[codex_models::ModelInfo],
    current: &review_mcp::CodexReviewConfig,
) -> usize {
    match &current.model {
        None => 0,
        Some(slug) => rows
            .iter()
            .position(|m| &m.slug == slug)
            .map(|i| i + 1)
            .unwrap_or(rows.len() + 1), // Custom row (last)
    }
}

/// Pure: the (model, ctx) args to persist for a chosen row, or `None` when the choice
/// needs free-text input first (Custom). The context window is PRESERVED for Model rows
/// (never silently cleared — it is a CLI-only setting); only Default clears both.
fn model_choice_set_args(
    choice: &ModelChoice,
    current_ctx: Option<u64>,
) -> Option<(Option<String>, Option<u64>)> {
    match choice {
        ModelChoice::Default => Some((None, None)),
        ModelChoice::Model(m) => Some((Some(m.slug.clone()), current_ctx)),
        ModelChoice::Custom => None,
    }
}

/// Pure: feed one key to the custom-slug editor. Only slug-safe chars (matching
/// `review_mcp`'s validation: ascii-alphanumeric + `.`/`_`/`-`) are accepted, so a typed
/// id can never be shown-then-rejected on save. Enter submits the trimmed buffer; Esc
/// cancels; every other key leaves the buffer unchanged.
fn edit_model_input(buf: &mut String, code: KeyCode) -> InputOutcome {
    match code {
        KeyCode::Char(c) if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') => {
            buf.push(c);
            InputOutcome::Editing
        }
        KeyCode::Backspace => {
            buf.pop();
            InputOutcome::Editing
        }
        KeyCode::Enter => InputOutcome::Submit(buf.trim().to_string()),
        KeyCode::Esc => InputOutcome::Cancel,
        _ => InputOutcome::Editing,
    }
}

/// A finished background discovery: (server name, discovered tools or error).
type DiscoverResult = (String, Result<Vec<String>, String>);

/// Outcome of a self-update probe (Codex R3 B1+B6 typed-state replacement for
/// the v0.20.0 string-matching `update_line` heuristic).
#[derive(Debug, Clone, PartialEq, Eq)]
enum SelfUpdateState {
    /// Initial state — no probe has run yet for this TUI session.
    Unprobed,
    /// A background probe is in flight (`update_rx` is `Some`).
    Checking,
    /// Probe returned `Skip { reason }` — already on latest, no release published, etc.
    UpToDate { reason: String },
    /// Probe returned `Apply(PlannedUpdate)` — newer release is available + valid.
    /// Carries the full `PlannedUpdate` so the staged-update flow has the target
    /// path + tag without re-planning (v0.25.0).
    Newer {
        from: String,
        to: String,
        summary: String,
        planned: aibridge_core::update::PlannedUpdate,
    },
    /// Probe failed (network, gh auth, missing asset, version parse). NOT updatable.
    Error { detail: String },
}

/// Typed result sent from the probe worker back to the UI thread.
type SelfUpdateProbeResult = Result<aibridge_core::update::UpdateDecision, String>;

/// Injectable self-update planner. Production uses `aibridge_core::update::plan_update`;
/// tests pass a closure that returns a synthetic `UpdateDecision` so no network
/// call happens during `cargo test`. (Codex R3 B3.)
type Planner = std::sync::Arc<
    dyn Fn(&aibridge_core::update::ApplyOptions) -> SelfUpdateProbeResult + Send + Sync,
>;

/// v0.25.0: injectable starter for the in-TUI self-update STAGING worker. Given the
/// planned update, it spawns the work (download+verify+stage+spawn detached helper)
/// on a background thread and returns a channel that yields the final `Result`.
/// Production wires the real `staged_update` flow; tests inject a fake.
type SelfStageStarter = std::sync::Arc<
    dyn Fn(aibridge_core::update::PlannedUpdate) -> std::sync::mpsc::Receiver<Result<(), String>>
        + Send
        + Sync,
>;

/// Production staging starter: stage + spawn the detached helper on a worker thread.
fn production_self_stage_starter() -> SelfStageStarter {
    std::sync::Arc::new(|planned: aibridge_core::update::PlannedUpdate| {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let res = (|| {
                let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
                let staged = aibridge_core::staged_update::stage_planned_update(&planned, &exe)?;
                // v0.28: single platform dispatcher (also used by retry). macOS applies
                // IMMEDIATELY (replacing a running binary in place is safe on Unix → a reload
                // picks it up); Windows/Linux spawn the detached wait-for-exit helper. The
                // choice is compile-time (cfg) inside activate_staged_update.
                aibridge_core::staged_update::activate_staged_update(&staged)
            })();
            let _ = tx.send(res);
        });
        rx
    })
}

/// A finished Claude-side discovery: (server name, tools or error). Same shape as the
/// codex `DiscoverResult` but kept separate so a late codex-tab message can't poison
/// the Claude inspector's per-tool view (and vice versa).
type ClaudeDiscoverResult = (String, Result<Vec<String>, String>);

/// v0.22.0: stream of events from an in-TUI CLI-update worker thread. Drained
/// on each tick. `Started` is fired once, `Stage(_)` once per phase, then a
/// terminal `Done(Ok|Err)`.
#[derive(Debug, Clone)]
enum CliUpdateEvent {
    Started,
    Stage(String),
    Done(Result<String, String>),
}

/// v0.22.0: injectable starter for the background CLI-checks worker (codex /
/// claude / rtk read-only version probes). Production wraps the existing
/// spawn-and-send pattern; tests inject a closure returning a pre-filled or
/// immediately-disconnected receiver so unit tests NEVER shell out to
/// `gh`/`brew`/`npm`. Production callers still go through `start_cli_checks`
/// which preserves the in-flight early-return guard.
type CliChecksStarter = std::sync::Arc<
    dyn Fn() -> std::sync::mpsc::Receiver<Vec<aibridge_core::cli_update::CliCheck>> + Send + Sync,
>;

/// v0.22.0: injectable starter for in-TUI CLI updates. Production spawns a real
/// thread calling `rtk::install_or_update_native_with_progress`; tests inject a
/// closure that returns a pre-filled receiver so NO real `gh` / FS calls happen
/// during `cargo test`.
type CliUpdateStarter = std::sync::Arc<
    dyn Fn(
            aibridge_core::cli_update::RtkNativeAction,
        ) -> (
            std::sync::mpsc::Receiver<CliUpdateEvent>,
            std::thread::JoinHandle<()>,
        ) + Send
        + Sync,
>;

/// v0.25.0: injectable starter for a GENERIC in-TUI CLI update — streams a
/// package-manager command's output (codex/claude/brew/npm, and brew-rtk) into the
/// dashboard instead of dropping to a shell. Production runs
/// `cli_update::apply_cli_update_streaming` on a worker thread; tests inject a fake.
type CliStreamStarter = std::sync::Arc<
    dyn Fn(
            &'static str,
            Vec<String>,
        ) -> (
            std::sync::mpsc::Receiver<CliUpdateEvent>,
            std::thread::JoinHandle<()>,
        ) + Send
        + Sync,
>;

/// Production generic CLI-stream starter: spawns a thread that streams `argv`'s
/// output line-by-line as `Stage` events, then a terminal `Done` keyed on exit code.
fn production_cli_stream_starter() -> CliStreamStarter {
    std::sync::Arc::new(|tool: &'static str, argv: Vec<String>| {
        let (tx, rx) = std::sync::mpsc::channel();
        let handle = std::thread::spawn(move || {
            let _ = tx.send(CliUpdateEvent::Started);
            let tx_line = tx.clone();
            let on_line = move |line: String| {
                let _ = tx_line.send(CliUpdateEvent::Stage(line));
            };
            let result = match aibridge_core::cli_update::apply_cli_update_streaming(&argv, on_line)
            {
                Ok(0) => Ok(format!("{tool} updated")),
                Ok(code) => Err(format!("{tool} exited with code {code}")),
                Err(e) => Err(format!("{tool} update failed: {e}")),
            };
            let _ = tx.send(CliUpdateEvent::Done(result));
        });
        (rx, handle)
    })
}

/// v0.27.0: injectable opener for the macOS Homebrew Terminal-handoff. Production
/// writes the FIXED `homebrew_handoff_script` to a private 0700 temp `.command` and
/// hands it to Apple's Terminal via `aibridge_platform::open_homebrew_install_terminal`
/// — Terminal (not AI Bridge) runs the installer and takes the sudo password. Tests
/// inject a fake that records the call without spawning Terminal. `Ok(())` ⇒ handed off.
type HomebrewOpener = std::sync::Arc<dyn Fn() -> Result<(), String> + Send + Sync>;

/// Production Homebrew opener: build the fixed handoff script + open Terminal.
fn production_homebrew_opener() -> HomebrewOpener {
    std::sync::Arc::new(|| {
        let script = aibridge_core::cli_update::homebrew_handoff_script();
        aibridge_platform::open_homebrew_install_terminal(&script).map_err(|e| e.to_string())
    })
}

/// v0.22.0: in-TUI background CLI-update run. Single in-flight per `App`.
/// `q` is blocked while `finished.is_none()`. On `Done`, the result moves to
/// `App::last_cli_run_result` and this slot is cleared so the user can press
/// `u` again immediately.
struct ActiveCliRun {
    tool: &'static str,
    rx: std::sync::mpsc::Receiver<CliUpdateEvent>,
    latest_stage: String,
    #[allow(dead_code)] // for future "elapsed time" rendering
    started_at: Instant,
    finished: Option<Result<String, String>>,
    /// Kept so the JoinHandle is dropped (and the worker is reaped) on App drop.
    /// Never `.join()`ed at runtime — worker runs to completion; we just consume
    /// events.
    #[allow(dead_code)]
    handle: Option<std::thread::JoinHandle<()>>,
}

/// v0.22.0: result of the most recent in-TUI CLI update, kept after
/// `active_cli_run` is cleared so the inline row decoration (`✓ updated` /
/// `⚠ <err>`) persists until the user re-presses `u` for the same tool.
struct LastCliRunResult {
    tool: &'static str,
    outcome: Result<String, String>,
    #[allow(dead_code)] // for future "completed N seconds ago" rendering
    at: Instant,
}

/// What a background `bump_prepare` or `bump_commit` produced.
enum BumpFlowMsg {
    /// Staging completed → here's the preview; show it and wait for the SECOND `B`.
    Prepared(aibridge_core::managed_skills::BumpPreview),
    /// Either preparing or committing failed → display this error in the footer.
    Failed(String),
    /// Committing completed → display this result and refresh.
    Committed(aibridge_core::managed_skills::OpResult),
}

/// Dashboard state: data snapshots + selection. Rendering and IO live in free
/// functions / refresh methods so the navigation logic stays pure + testable.
struct App {
    cwd: String,
    tab: Tab,
    checks: Vec<Check>,
    health_scroll: u16,
    review: Option<serde_json::Value>,
    review_summary: Option<String>,
    /// v0.29 (O1c) review-model picker. `model_rows` = codex's live model cache
    /// (read on launch + `r`); `model_current` = the persisted review-mcp.json config
    /// (what's CONFIGURED, possibly not yet live in the running server); `model_sel` =
    /// the highlighted picker row; `model_input` = Some(buffer) while typing a custom
    /// slug (None = list mode).
    model_rows: Vec<codex_models::ModelInfo>,
    model_current: review_mcp::CodexReviewConfig,
    model_sel: usize,
    model_input: Option<String>,
    /// `None` ⇒ the codex config is present but unenumerable (fail-closed); reviews
    /// would be refused — surfaced as a warning in the MCP tab.
    mcp: Option<Vec<McpRow>>,
    mcp_sel: usize,
    /// Codex-MCP per-tool view (after Enter on a server): which server is open, its
    /// cached tools (no relaunch), whether the cache is fresh + known, and the
    /// in-flight discovery (background thread → channel) so 'd' never blocks the loop.
    mcp_view: McpView,
    mcp_server: Option<String>,
    tool_rows: Vec<(String, bool)>,
    tools_fresh: bool,
    tools_known: bool,
    tool_sel: usize,
    discover_rx: Option<std::sync::mpsc::Receiver<DiscoverResult>>,
    discovering: Option<String>,
    /// Update tab: the current display line (version + last check/result) and an
    /// in-flight check (background thread → channel). v0.25.0: the self-update no
    /// longer exits the TUI — it STAGES the new binary and a detached helper applies
    /// it once all aibridge processes exit (see `self_stage_*`).
    update_line: String,
    update_rx: Option<std::sync::mpsc::Receiver<SelfUpdateProbeResult>>,
    /// v0.25.0: in-flight self-update STAGING (stage + spawn detached helper) on a
    /// background thread → channel. `Some` while staging is running.
    self_stage_rx: Option<std::sync::mpsc::Receiver<Result<(), String>>>,
    /// v0.25.0: injectable starter for the staging worker. Production stages + spawns
    /// the detached helper; tests inject a fake that pre-fills the channel.
    self_stage_starter: SelfStageStarter,
    /// v0.25.0: cached staged-update status (refreshed on the poll tick) so the
    /// Update tab can show staged/waiting/✓/⚠ + offer cancel (Waiting) / retry (Failed)
    /// without a file read every frame.
    staged_status: Option<aibridge_core::staged_update::UpdateStatus>,
    /// v0.20.1 (Codex R3 B1+B6): typed self-update state machine driving the
    /// `u` keypress on row 0. Only `Newer` lets the TUI exit + apply; every
    /// other state shows a footer message without quitting (fixes the UX where
    /// pressing `u` while on the latest release dumped the user to the shell).
    self_update_state: SelfUpdateState,
    /// One-shot guard so the lazy auto-probe fires exactly once on first Update
    /// tab entry. Manual `c` reruns are independent of this.
    update_check_kicked: bool,
    /// Clipboard writer for the Debug + Health tab `y` key. Injectable so tests
    /// don't mutate the real system clipboard.
    clipboard: Box<dyn aibridge_platform::ClipboardWriter>,
    /// Injectable self-update planner. Production reads real GitHub releases via
    /// `plan_update`; tests inject a closure returning a synthetic `UpdateDecision`
    /// so the `c` key + first-view auto-trigger never touch the network in tests.
    planner: Planner,
    /// CLI-update rows (codex / claude / rtk). Loaded on a background thread the
    /// first time the Update tab is entered and on `r`. Each row drives `u` in
    /// concert with `update_sel`. Codex Stop-gate R6: mutation runs AFTER TUI
    /// exit via `pending_cli_updates`; never inside the alt-screen.
    cli_checks: Vec<aibridge_core::cli_update::CliCheck>,
    cli_checks_rx: Option<std::sync::mpsc::Receiver<Vec<aibridge_core::cli_update::CliCheck>>>,
    /// MCP version-pin status (read-only). Loaded alongside `cli_checks`.
    mcp_pins: Vec<aibridge_core::cli_update::McpVersionStatus>,
    /// Selected row on the Update tab. Index space is: 0=SelfAibridge, 1..=cli,
    /// remaining=MCP pins.
    update_sel: usize,
    /// v0.22.0: starter for in-TUI rtk-native updates (Update or Install). Production
    /// spawns a real worker thread; tests inject a fake that pre-fills events.
    cli_update_starter: CliUpdateStarter,
    /// v0.25.0: generic in-TUI CLI-stream starter (codex/claude/brew/npm, brew-rtk).
    /// Streams the package-manager command's output into the dashboard — no shell drop.
    cli_stream_starter: CliStreamStarter,
    /// v0.25.0: 2-key confirm for a FreshInstall CLI row (first global install).
    /// First `u` arms the tool name; second `u` on the same row starts the in-TUI
    /// stream. Any other key / tab change / different row clears it.
    fresh_install_armed: Option<&'static str>,
    /// v0.27.0: injectable Homebrew Terminal-handoff opener (macOS only). Production
    /// opens Apple's Terminal with the fixed installer; tests inject a recording fake.
    homebrew_opener: HomebrewOpener,
    /// v0.27.0: 2-key confirm for the `b` (install Homebrew) affordance. First `b`
    /// arms; second `b` opens Terminal. Any other key clears it (top-of-`handle_key`
    /// guard), mirroring `fresh_install_armed`.
    brew_install_armed: bool,
    /// v0.22.0: in-flight in-TUI CLI update (rtk-native only). When `Some` and
    /// `finished.is_none()`, `q` is blocked and `u` is a no-op (single in-flight).
    active_cli_run: Option<ActiveCliRun>,
    /// v0.22.0: most recent in-TUI CLI update result, for the inline `✓`/`⚠`
    /// decoration on the row. Cleared when the user re-presses `u` for that tool.
    last_cli_run_result: Option<LastCliRunResult>,
    /// v0.22.0 (Codex code-gate B2): when a CLI update Done event arrives WHILE
    /// `cli_checks_rx` is already in flight, `start_cli_checks` early-returns,
    /// which would skip the post-update re-probe. Set this flag instead; the
    /// existing rx drain in `poll_update` clears it AFTER consuming the in-flight
    /// result and kicks a fresh `start_cli_checks` so the version row reflects
    /// the newly-installed CLI.
    pending_cli_recheck: bool,
    /// v0.22.0 (Codex code-gate B2 R2): injectable starter for the background
    /// CLI-checks worker. Production wraps the existing real spawn-and-send;
    /// tests inject a fake so unit tests don't shell out.
    cli_checks_starter: CliChecksStarter,
    /// Skills tab: the (lazily-computed) personal-skills `skills::doctor()` text. Kept
    /// for backward-compat with the Health rendering; managed-skills now own the tab UI.
    skills_report: Option<String>,
    /// Pending 2-key confirm for a MUTATING Skills action — letters (`s`/`m`/`i`/`n`/`d`/
    /// `x`/`p`/`o`) and a sentinel `\n` for Enter (the per-skill install). The action only
    /// runs on the SECOND matching press; any other key cancels it.
    skills_confirm: Option<char>,
    /// In-flight `managed apply` (background thread → channel) so the network fetch never
    /// blocks the event loop; the result line folds back into the footer.
    managed_apply_rx: Option<std::sync::mpsc::Receiver<String>>,
    /// One row per managed skill (loaded OFFLINE via `statuses()` on refresh).
    managed_rows: Vec<aibridge_core::managed_skills::SkillStatus>,
    /// Currently-selected row in the managed list (clamped on refresh).
    managed_sel: usize,
    /// `Some(err)` ⇒ the manifest is missing/invalid (the list shows the hint to `n` init).
    managed_error: Option<String>,
    /// `U` check-upstream: in-flight network probe (background thread → channel) + the
    /// last-fetched candidates by skill name (so the UI can annotate rows + `B` can use them).
    upstream_rx:
        Option<std::sync::mpsc::Receiver<Vec<aibridge_core::managed_skills::UpstreamCandidate>>>,
    upstream_candidates:
        std::collections::HashMap<String, aibridge_core::managed_skills::UpstreamCandidate>,
    /// `B` two-phase: the staged preview from `bump_prepare`. First `B` press stages and
    /// puts the preview here; second `B` press commits (writes manifest + applies). Drop
    /// (on cancel or successful commit) cleans the staged temp.
    pending_bump: Option<aibridge_core::managed_skills::BumpPreview>,
    /// `B`/`U` in-flight on a background thread (lock would freeze the UI if held in the
    /// event loop).
    bump_rx: Option<std::sync::mpsc::Receiver<BumpFlowMsg>>,
    /// Claude MCP Inspector — read-only inventory + per-tool discovery. v1 management
    /// (toggle/edit/restart-nudge) is a follow-up plan; mutating keys no-op here with
    /// a "view-only" footer message so the user gets feedback, not silence.
    claude_inventory: Option<Vec<claude_mcp::ClaudeServer>>,
    claude_inventory_error: Option<String>,
    /// Non-fatal warnings from the Claude MCP merge (e.g. one source corrupt while
    /// the other loaded). Codex Stop-gate F2: surfaced ABOVE the empty-state hint
    /// so a corrupt `~/.claude.json` plus an empty project `.mcp.json` no longer
    /// silently reports "no MCPs configured".
    claude_warnings: Vec<String>,
    claude_sel: usize,
    claude_view: McpView,
    claude_server: Option<claude_mcp::ClaudeServer>,
    claude_tool_rows: Vec<String>,
    claude_tools_known: bool,
    claude_tool_sel: usize,
    claude_discover_rx: Option<std::sync::mpsc::Receiver<ClaudeDiscoverResult>>,
    claude_discovering: Option<String>,
    /// Debug tab — assembled lazily on first view + on `r`. The report is built on a
    /// worker thread (the codex handshake inside `doctor::run` can take a few seconds)
    /// so the UI keeps drawing. While in flight, `debug_text` shows the building
    /// notice.
    debug_text: Option<String>,
    debug_rx: Option<std::sync::mpsc::Receiver<String>>,
    debug_scroll: u16,
    message: Option<String>,
    quit: bool,
}

/// v0.22.0: production CLI-checks worker starter. Spawns a thread that runs
/// `cli_update::check_all` with `RealCommandRunner` and sends the result back
/// over an mpsc channel — same behavior as the prior inline `start_cli_checks`
/// body. NEVER called in tests; tests inject a fake.
fn production_cli_checks_starter() -> CliChecksStarter {
    std::sync::Arc::new(|| {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let runner = aibridge_core::cli_update::RealCommandRunner;
            let _ = tx.send(aibridge_core::cli_update::check_all(&runner));
        });
        rx
    })
}

/// v0.22.0: production rtk-native worker starter. Spawns a thread that calls
/// `rtk::install_or_update_native_with_progress` with real runner/resolver/
/// downloader/fs and forwards stage strings + the final result over an mpsc
/// channel. NEVER called in tests — tests inject a fake starter.
fn production_rtk_starter() -> CliUpdateStarter {
    std::sync::Arc::new(|action: aibridge_core::cli_update::RtkNativeAction| {
        use aibridge_core::cli_update::{RealCommandRunner, RealPathResolver, RtkNativeAction};
        use aibridge_core::rtk::{
            install_or_update_native_with_progress, GhReleaseDownloader, InstallOpts, RealFsOps,
        };
        let (tx, rx) = std::sync::mpsc::channel();
        let opts = InstallOpts {
            yes: true,
            allow_fresh_install: matches!(action, RtkNativeAction::Install),
        };
        let handle = std::thread::spawn(move || {
            let _ = tx.send(CliUpdateEvent::Started);
            let tx_stage = tx.clone();
            let on_stage = move |s: &str| {
                let _ = tx_stage.send(CliUpdateEvent::Stage(s.to_string()));
            };
            let result = install_or_update_native_with_progress(
                &RealCommandRunner,
                &RealPathResolver,
                &GhReleaseDownloader,
                &RealFsOps,
                opts,
                &on_stage,
            );
            let _ = tx.send(CliUpdateEvent::Done(result));
        });
        (rx, handle)
    })
}

impl App {
    fn new(cwd: String) -> Self {
        let mut app = App {
            cwd,
            tab: Tab::Health,
            checks: Vec::new(),
            health_scroll: 0,
            review: None,
            review_summary: None,
            model_rows: Vec::new(),
            model_current: review_mcp::CodexReviewConfig::default(),
            model_sel: 0,
            model_input: None,
            mcp: None,
            mcp_sel: 0,
            mcp_view: McpView::Servers,
            mcp_server: None,
            tool_rows: Vec::new(),
            tools_fresh: false,
            tools_known: false,
            tool_sel: 0,
            discover_rx: None,
            discovering: None,
            update_line: "Press 'c' to check for a newer release.".to_string(),
            update_rx: None,
            self_stage_rx: None,
            self_stage_starter: production_self_stage_starter(),
            staged_status: None,
            self_update_state: SelfUpdateState::Unprobed,
            update_check_kicked: false,
            clipboard: aibridge_platform::real_clipboard(),
            planner: std::sync::Arc::new(aibridge_core::update::plan_update),
            cli_checks: Vec::new(),
            cli_checks_rx: None,
            mcp_pins: Vec::new(),
            update_sel: 0,
            cli_update_starter: production_rtk_starter(),
            cli_stream_starter: production_cli_stream_starter(),
            fresh_install_armed: None,
            homebrew_opener: production_homebrew_opener(),
            brew_install_armed: false,
            active_cli_run: None,
            last_cli_run_result: None,
            pending_cli_recheck: false,
            cli_checks_starter: production_cli_checks_starter(),
            skills_report: None,
            skills_confirm: None,
            managed_apply_rx: None,
            managed_rows: Vec::new(),
            managed_sel: 0,
            managed_error: None,
            upstream_rx: None,
            upstream_candidates: std::collections::HashMap::new(),
            pending_bump: None,
            bump_rx: None,
            claude_inventory: None,
            claude_inventory_error: None,
            claude_warnings: Vec::new(),
            claude_sel: 0,
            claude_view: McpView::Servers,
            claude_server: None,
            claude_tool_rows: Vec::new(),
            claude_tools_known: false,
            claude_tool_sel: 0,
            claude_discover_rx: None,
            claude_discovering: None,
            debug_text: None,
            debug_rx: None,
            debug_scroll: 0,
            message: None,
            quit: false,
        };
        app.refresh_all();
        // O1c: load the review-model picker (live cache + persisted config) and open
        // highlighting the configured row.
        app.refresh_review_models();
        app.realign_model_sel();
        app
    }

    /// Reload the Claude MCP inventory (offline; just reads ~/.claude.json + the
    /// closest-ancestor `.mcp.json`). Warnings from `merge_inventory` flow into
    /// `claude_warnings` so the Inspector renders them even when there are no
    /// servers to list.
    fn refresh_claude(&mut self) {
        match claude_mcp::inventory(std::path::Path::new(&self.cwd)) {
            claude_mcp::Inventory::Available { servers, warnings } => {
                self.claude_inventory = Some(servers);
                self.claude_inventory_error = None;
                self.claude_warnings = warnings;
            }
            claude_mcp::Inventory::Unavailable(why) => {
                self.claude_inventory = None;
                self.claude_inventory_error = Some(why);
                self.claude_warnings = Vec::new();
            }
        }
        if let Some(rows) = &self.claude_inventory {
            if self.claude_sel >= rows.len() {
                self.claude_sel = rows.len().saturating_sub(1);
            }
        }
    }

    /// Enter the selected Claude server's per-tool view.
    fn open_selected_claude(&mut self) {
        if self.tab != Tab::ClaudeMcpInspector || self.claude_view != McpView::Servers {
            return;
        }
        let Some(server) = self
            .claude_inventory
            .as_ref()
            .and_then(|rows| rows.get(self.claude_sel))
            .cloned()
        else {
            return;
        };
        self.claude_server = Some(server);
        self.claude_view = McpView::Tools;
        self.claude_tool_sel = 0;
        self.claude_tool_rows.clear();
        self.claude_tools_known = false;
    }

    /// Esc: leave the per-tool view back to the server list.
    fn claude_back(&mut self) -> bool {
        if self.tab == Tab::ClaudeMcpInspector && self.claude_view == McpView::Tools {
            self.claude_view = McpView::Servers;
            self.claude_server = None;
            self.claude_tool_rows.clear();
            self.claude_tools_known = false;
            true
        } else {
            false
        }
    }

    /// 'd' in the Claude Inspector's per-tool view: discover the open server's tools on
    /// a BACKGROUND thread. Goes through `claude_mcp::discover` which uses
    /// `tool_discovery::discover` (the UNCACHED entry point) so the codex cache is
    /// never touched.
    fn start_claude_discover(&mut self) {
        if self.tab != Tab::ClaudeMcpInspector
            || self.claude_view != McpView::Tools
            || self.claude_discovering.is_some()
        {
            return;
        }
        let Some(server) = self.claude_server.clone() else {
            return;
        };
        let name = server.name.clone();
        let cwd = self.cwd.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        self.claude_discover_rx = Some(rx);
        self.claude_discovering = Some(name.clone());
        self.message = Some(format!(
            "Discovering '{name}' (Claude side) — launching it briefly..."
        ));
        std::thread::spawn(move || {
            let r = claude_mcp::discover(&server, std::path::Path::new(&cwd));
            let _ = tx.send((name, r));
        });
    }

    fn poll_claude_discover(&mut self) {
        let Some(rx) = &self.claude_discover_rx else {
            return;
        };
        if let Ok((server, res)) = rx.try_recv() {
            self.claude_discover_rx = None;
            self.claude_discovering = None;
            if self.claude_server.as_ref().map(|s| s.name.clone()) == Some(server.clone()) {
                match res {
                    Ok(tools) => {
                        self.claude_tool_rows = tools;
                        self.claude_tools_known = true;
                        self.message = Some(format!(
                            "Discovered {} tool(s) of '{server}' (Claude side).",
                            self.claude_tool_rows.len()
                        ));
                    }
                    Err(e) => {
                        self.claude_tool_rows.clear();
                        self.claude_tools_known = true;
                        self.message = Some(format!("Discovery of '{server}' failed: {e}"));
                    }
                }
            }
        }
    }

    /// Footer message for any mutating key the Claude Inspector intercepts. Kept as a
    /// single string so the v2 management plan can rewrite it in one place.
    fn claude_view_only_message(&mut self) {
        self.message = Some(
            "view-only — to toggle, edit ~/.claude.json (or the project's .mcp.json) and \
             restart Claude. Management UX is a follow-up."
                .into(),
        );
    }

    /// Kick off (or re-run on `r`) the Debug-tab report build on a worker thread. The
    /// `doctor::debug_report` call spawns codex briefly + queries the Claude inventory,
    /// so doing it on the event loop would freeze the UI for a few seconds.
    fn start_debug_build(&mut self) {
        if self.debug_rx.is_some() {
            return;
        }
        let cwd = self.cwd.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        self.debug_rx = Some(rx);
        self.debug_text = Some(
            "Building debug report (this can take a few seconds while the Codex handshake runs)..."
                .to_string(),
        );
        std::thread::spawn(move || {
            let report = doctor::debug_report(std::path::Path::new(&cwd));
            let _ = tx.send(report);
        });
    }

    fn poll_debug_build(&mut self) {
        if let Some(rx) = &self.debug_rx {
            if let Ok(text) = rx.try_recv() {
                self.debug_text = Some(text);
                self.debug_rx = None;
            }
        }
    }

    /// (Re)compute the Skills doctor reports (folder digests → not on the fast tick):
    /// the personal-skills doctor (the bottom panel) AND the per-skill managed status list
    /// (the top panel — interactive). Both are OFFLINE.
    fn refresh_skills(&mut self) {
        self.skills_report = Some(skills::doctor());
        match aibridge_core::managed_skills::statuses() {
            Ok(rows) => {
                self.managed_rows = rows;
                self.managed_error = None;
            }
            Err(e) => {
                self.managed_rows = Vec::new();
                self.managed_error = Some(e);
            }
        }
        if self.managed_sel >= self.managed_rows.len() {
            self.managed_sel = self.managed_rows.len().saturating_sub(1);
        }
    }

    /// Start a background `managed apply` for the given target (ALL or a single skill name).
    /// `apply` is the only networked managed action, so it runs off the event loop.
    /// Idempotent in flight.
    fn start_managed_apply(
        &mut self,
        target: aibridge_core::managed_skills::Target,
        repair: bool,
        adopt: bool,
    ) {
        if self.managed_apply_rx.is_some() {
            return;
        }
        let label = match &target {
            aibridge_core::managed_skills::Target::All => "all".to_string(),
            aibridge_core::managed_skills::Target::One(n) => n.clone(),
        };
        let mut flags = Vec::new();
        if repair {
            flags.push("--repair");
        }
        if adopt {
            flags.push("--adopt");
        }
        let flag_str = if flags.is_empty() {
            String::new()
        } else {
            format!(" {}", flags.join(" "))
        };
        let (tx, rx) = std::sync::mpsc::channel();
        self.managed_apply_rx = Some(rx);
        self.message = Some(format!(
            "Applying managed skills{flag_str} ({label}) — fetching pinned sources..."
        ));
        std::thread::spawn(move || {
            let r = aibridge_core::managed_skills::apply(target, repair, adopt);
            let _ = tx.send(r.message);
        });
    }

    /// `n` in TUI → write a starter manifest (no fetch, no install). Refreshes after.
    fn managed_init(&mut self) {
        let msg = aibridge_core::managed_skills::init();
        self.message = Some(msg.lines().next().unwrap_or("init").to_string());
        self.refresh_skills();
    }

    /// `d` in TUI → disable the selected managed skill. Synchronous (filesystem-only);
    /// the core acquires the process lock, so a CLI apply in another terminal would block
    /// here — but the TUI's own apply-in-flight guard prevents that locally.
    fn managed_disable_selected(&mut self) {
        let Some(name) = self.selected_managed() else {
            return;
        };
        let r = aibridge_core::managed_skills::disable(&name);
        // On a partial, SURFACE the loud ⚠ line so the user actually sees the failure.
        self.message = Some(format_managed_message(&format!("disable {name}"), &r));
        self.refresh_skills();
    }

    /// `x` in TUI → remove the selected managed skill (synchronous; same lock semantics).
    fn managed_remove_selected(&mut self) {
        let Some(name) = self.selected_managed() else {
            return;
        };
        let r = aibridge_core::managed_skills::remove(&name);
        self.message = Some(format_managed_message(&format!("remove {name}"), &r));
        self.refresh_skills();
    }

    fn selected_managed(&self) -> Option<String> {
        self.managed_rows
            .get(self.managed_sel)
            .map(|s| s.name.clone())
    }

    /// `M` in TUI → migrate-and-install the selected skill: quarantine the foreign mirror(s)
    /// into ~/.ai-bridge/backups/ (reversible) and apply. Runs on a background thread
    /// because the apply phase fetches the pinned source.
    fn start_managed_migrate(&mut self) {
        let Some(name) = self.selected_managed() else {
            return;
        };
        if self.managed_apply_rx.is_some() {
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        self.managed_apply_rx = Some(rx);
        self.message = Some(format!(
            "Migrate-and-install {name} — quarantining foreign + fetching pinned source..."
        ));
        std::thread::spawn(move || {
            let r = aibridge_core::managed_skills::migrate_and_install(&name);
            let _ = tx.send(r.message);
        });
    }

    /// `U` in TUI → probe upstream for every enabled+tracked git skill. Network on a
    /// background thread; results fold into `upstream_candidates` so rows can annotate.
    fn start_check_upstream(&mut self) {
        if self.upstream_rx.is_some() {
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        self.upstream_rx = Some(rx);
        self.message = Some("Probing upstream for every tracked managed skill...".into());
        std::thread::spawn(move || {
            let cands = aibridge_core::managed_skills::check_upstream();
            let _ = tx.send(cands);
        });
    }

    fn poll_check_upstream(&mut self) {
        if let Some(rx) = &self.upstream_rx {
            if let Ok(cands) = rx.try_recv() {
                self.upstream_rx = None;
                let probed = cands.len();
                let mut updates = 0;
                self.upstream_candidates.clear();
                for c in cands {
                    if c.update_available() {
                        updates += 1;
                    }
                    self.upstream_candidates.insert(c.name.clone(), c);
                }
                self.message = Some(format!(
                    "Upstream probe: {probed} tracked, {updates} update(s) available. \
                     Press B (twice) on a row to bump it."
                ));
            }
        }
    }

    /// `B` first press in TUI → stage the upstream candidate on a background thread; on
    /// completion, store the preview and surface its summary in the footer. A second `B`
    /// then commits (writes manifest + applies).
    fn start_bump_prepare(&mut self) {
        let Some(name) = self.selected_managed() else {
            return;
        };
        let Some(cand) = self.upstream_candidates.get(&name).cloned() else {
            self.message = Some(format!(
                "No upstream candidate for {name}. Press U first to probe (the skill needs `update_ref` in the manifest)."
            ));
            return;
        };
        let Some(new_sha) = cand.upstream_sha else {
            self.message = Some(format!(
                "Upstream probe for {name} failed earlier (network/auth?) — press U again."
            ));
            return;
        };
        if new_sha == cand.current_sha {
            self.message = Some(format!("{name} is already at upstream — nothing to bump."));
            return;
        }
        if self.bump_rx.is_some() {
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        self.bump_rx = Some(rx);
        self.message = Some(format!(
            "Staging upstream {} for {name}...",
            &new_sha[..new_sha.len().min(8)]
        ));
        std::thread::spawn(move || {
            let msg = match aibridge_core::managed_skills::bump_prepare(&name, &new_sha) {
                Ok(p) => BumpFlowMsg::Prepared(p),
                Err(e) => BumpFlowMsg::Failed(e),
            };
            let _ = tx.send(msg);
        });
    }

    /// `B` second press → commit the held preview (writes manifest ref + apply).
    fn start_bump_commit(&mut self) {
        let Some(preview) = self.pending_bump.take() else {
            return;
        };
        if self.bump_rx.is_some() {
            self.pending_bump = Some(preview);
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        self.bump_rx = Some(rx);
        let label = preview.summary();
        self.message = Some(format!(
            "Committing bump {label} — writing manifest + applying..."
        ));
        std::thread::spawn(move || {
            let r = aibridge_core::managed_skills::bump_commit(preview);
            let _ = tx.send(BumpFlowMsg::Committed(r));
        });
    }

    fn poll_bump(&mut self) {
        if let Some(rx) = &self.bump_rx {
            if let Ok(msg) = rx.try_recv() {
                self.bump_rx = None;
                match msg {
                    BumpFlowMsg::Prepared(preview) => {
                        let summary = preview.summary();
                        self.pending_bump = Some(preview);
                        self.message = Some(format!(
                            "Bump preview: {summary} — press B AGAIN to commit, any other key cancels."
                        ));
                    }
                    BumpFlowMsg::Failed(e) => {
                        self.pending_bump = None;
                        self.message = Some(format!("bump failed: {e}"));
                    }
                    BumpFlowMsg::Committed(r) => {
                        self.refresh_skills();
                        self.message = Some(format_managed_message("bump", &r));
                    }
                }
            }
        }
    }

    /// Drop any held bump preview (e.g. when the user navigates away).
    fn clear_pending_bump(&mut self) {
        if self.pending_bump.is_some() {
            self.pending_bump = None;
            self.message = Some("Bump preview cancelled.".into());
        }
    }

    /// Poll the in-flight managed apply; on completion refresh the report + surface a line.
    fn poll_managed_apply(&mut self) {
        if let Some(rx) = &self.managed_apply_rx {
            if let Ok(out) = rx.try_recv() {
                self.managed_apply_rx = None;
                self.refresh_skills();
                let key = out
                    .lines()
                    .find(|l| l.contains(" : "))
                    .map(|l| l.trim().to_string())
                    .unwrap_or_else(|| "managed apply: done".into());
                self.message = Some(format!("managed apply done — {key} (reload Claude/Codex)"));
            }
        }
    }

    /// 2-key confirm gate for a mutating Skills action: returns true (and clears) on the
    /// SECOND matching press; otherwise arms `c` and returns false. Pure (unit-tested).
    fn skills_confirm_press(&mut self, c: char) -> bool {
        if self.skills_confirm == Some(c) {
            self.skills_confirm = None;
            true
        } else {
            self.skills_confirm = Some(c);
            false
        }
    }

    /// Apply sync / migrate (add-missing only; safe), refresh the report, surface the
    /// result's key line (incl. any failure) in the footer.
    fn apply_skills(&mut self, migrate: bool) {
        let r = if migrate {
            skills::migrate(true)
        } else {
            skills::sync(true)
        };
        self.refresh_skills();
        let key = r
            .lines()
            .find(|l| l.contains("ADD") || l.contains("nothing") || l.contains("failed"))
            .map(|l| l.trim().to_string())
            .unwrap_or_else(|| "done".into());
        let what = if migrate {
            "migrate (codex-legacy->hub)"
        } else {
            "sync (hub->agents)"
        };
        self.message = Some(format!("{what}: {key}"));
    }

    /// v0.20.1 Codex R3 B1: start a typed self-update probe on a worker thread.
    /// The result populates `self_update_state` so `handle_update_action` row-0
    /// dispatches on a typed enum (not a fragile string match). Idempotent while
    /// one is in flight.
    fn start_self_update_probe(&mut self) {
        if self.update_rx.is_some() {
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        self.update_rx = Some(rx);
        self.self_update_state = SelfUpdateState::Checking;
        self.update_line = "Checking for a newer release...".to_string();
        let planner = self.planner.clone();
        std::thread::spawn(move || {
            let opts = aibridge_core::update::ApplyOptions {
                assume_yes: true,
                from_source: false,
                target_path: None,
            };
            let _ = tx.send(planner(&opts));
        });
    }

    /// Poll the in-flight check; fold its result into the display line when ready.
    fn poll_update(&mut self) {
        if let Some(rx) = &self.update_rx {
            if let Ok(result) = rx.try_recv() {
                // Map the typed UpdateDecision into the typed SelfUpdateState.
                use aibridge_core::update::UpdateDecision;
                self.self_update_state = match result {
                    Ok(UpdateDecision::Skip { reason }) => {
                        self.update_line = reason.clone();
                        SelfUpdateState::UpToDate { reason }
                    }
                    Ok(UpdateDecision::Apply(p)) => {
                        let from = p
                            .from
                            .as_ref()
                            .map(|v| v.to_string())
                            .unwrap_or_else(|| "?".into());
                        let to = p.to.to_string();
                        let summary = format!("Newer release available: {from} → {to}");
                        self.update_line = summary.clone();
                        SelfUpdateState::Newer {
                            from,
                            to,
                            summary,
                            planned: p,
                        }
                    }
                    Err(detail) => {
                        self.update_line = format!("Self-update check failed: {detail}");
                        SelfUpdateState::Error { detail }
                    }
                };
                self.update_rx = None;
            }
        }
        // v0.25.0: drain the self-update STAGING worker.
        if let Some(rx) = &self.self_stage_rx {
            if let Ok(result) = rx.try_recv() {
                match result {
                    Ok(()) => {
                        self.update_line =
                            "Update staged ✓ — applies when all aibridge processes exit.".into();
                        self.message = Some(
                            "update staged ✓ — restart Claude Code (and close any aibridge \
                             processes) to apply it"
                                .into(),
                        );
                    }
                    Err(e) => {
                        self.update_line = format!("Staging failed: {e}");
                        self.message = Some(format!("staging failed: {e}"));
                    }
                }
                self.self_stage_rx = None;
            }
        }
        // v0.25.0: refresh the cached staged-update status for the Update tab.
        self.staged_status = aibridge_core::staged_update::read_update_status();
        if let Some(rx) = &self.cli_checks_rx {
            if let Ok(checks) = rx.try_recv() {
                self.cli_checks = checks;
                // Exact ordering matters (Codex code-gate B2 R2): clear rx FIRST
                // so `start_cli_checks` doesn't early-return; THEN check the
                // pending-rerun flag and kick a fresh check if needed.
                self.cli_checks_rx = None;
                if self.pending_cli_recheck {
                    self.pending_cli_recheck = false;
                    self.start_cli_checks();
                }
            }
        }
        // v0.22.0: drain the in-TUI CLI-update worker channel (rtk-native).
        // Non-blocking; on Done, move the result into `last_cli_run_result`
        // and clear `active_cli_run` so the user can re-press `u`.
        self.poll_active_cli_run();
    }

    /// v0.22.0: non-blocking drain of the active in-TUI CLI-update worker.
    /// Multiple events may arrive between ticks (Started → Stage → Stage → …
    /// → Done); we drain all available before returning. Triggers a background
    /// CLI re-check on the first observed `Done` so the row's `current` field
    /// reflects the new install.
    ///
    /// Disconnect handling (Codex code-gate B1): if all senders are dropped
    /// WITHOUT a terminal `Done` event (worker panic, OOM, etc.), the channel
    /// disconnects. Without explicit handling, `finished` would stay `None`
    /// and `q`/`Esc` would be blocked forever. We synthesize a `Done(Err(...))`
    /// so the lifecycle completes and the user can quit / retry.
    fn poll_active_cli_run(&mut self) {
        let mut just_finished_outcome: Option<Result<String, String>> = None;
        let mut just_finished_tool: Option<&'static str> = None;
        if let Some(run) = self.active_cli_run.as_mut() {
            loop {
                match run.rx.try_recv() {
                    Ok(CliUpdateEvent::Started) => {
                        run.latest_stage = "started".to_string();
                    }
                    Ok(CliUpdateEvent::Stage(s)) => {
                        run.latest_stage = s;
                    }
                    Ok(CliUpdateEvent::Done(result)) => {
                        run.finished = Some(result.clone());
                        just_finished_outcome = Some(result);
                        just_finished_tool = Some(run.tool);
                        break;
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        let synthetic = Err::<String, String>(
                            "worker exited without completing (panic or early drop)".to_string(),
                        );
                        run.finished = Some(synthetic.clone());
                        just_finished_outcome = Some(synthetic);
                        just_finished_tool = Some(run.tool);
                        break;
                    }
                }
            }
        }
        if let (Some(outcome), Some(tool)) = (just_finished_outcome, just_finished_tool) {
            self.last_cli_run_result = Some(LastCliRunResult {
                tool,
                outcome: outcome.clone(),
                at: Instant::now(),
            });
            let summary = match &outcome {
                Ok(msg) => {
                    let first = msg.lines().next().unwrap_or("").trim();
                    format!("{tool} updated: {first}")
                }
                Err(e) => {
                    let trimmed = e.chars().take(160).collect::<String>();
                    format!("{tool} update failed: {trimmed}")
                }
            };
            self.message = Some(summary);
            // Clear the active slot so `u` can be pressed again immediately.
            self.active_cli_run = None;
            // Schedule a background CLI re-check so the row's `current` reflects
            // the newly-installed version. If a check is already in flight,
            // `start_cli_checks` early-returns; set the deferred-rerun flag so
            // the rx drain in `poll_update` kicks another check on completion
            // (Codex code-gate B2).
            if self.cli_checks_rx.is_some() {
                self.pending_cli_recheck = true;
            } else {
                self.start_cli_checks();
            }
        }
    }

    /// Start a background CLI-checks worker (codex / claude / rtk). Read-only —
    /// no mutation. Idempotent while one is in flight. v0.22.0 (Codex code-gate
    /// B2 R2): spawn body extracted to the injectable `cli_checks_starter` so
    /// tests can drive lifecycle without shelling out.
    fn start_cli_checks(&mut self) {
        if self.cli_checks_rx.is_some() {
            return;
        }
        self.cli_checks_rx = Some((self.cli_checks_starter)());
        // MCP pins are cheap (local config only) — load synchronously.
        self.mcp_pins = aibridge_core::cli_update::scan_mcps(std::path::Path::new(&self.cwd));
    }

    /// Total rows on the Update tab: 1 (aibridge self) + N CLI checks + M MCP pins.
    fn update_row_count(&self) -> usize {
        1 + self.cli_checks.len() + self.mcp_pins.len()
    }

    /// Clamp `update_sel` into the valid range; saturate at 0 when empty.
    fn clamp_update_sel(&mut self) {
        let n = self.update_row_count();
        if n == 0 {
            self.update_sel = 0;
        } else if self.update_sel >= n {
            self.update_sel = n - 1;
        }
    }

    /// v0.20.1: copy the Debug report to clipboard. Per plan_gate R3 B4, refuses
    /// to copy a still-building placeholder. Footer message reports outcome.
    fn copy_debug_report_to_clipboard(&mut self) {
        if self.debug_rx.is_some() {
            self.message = Some("Debug report still building; try `y` again in a moment".into());
            return;
        }
        let Some(text) = self.debug_text.as_deref() else {
            self.message = Some("no Debug report yet — press `r` to build one".into());
            return;
        };
        match self.clipboard.copy(text) {
            Ok(n) => self.message = Some(format!("copied {n} bytes to clipboard")),
            Err(e) => self.message = Some(format!("clipboard write failed: {e}")),
        }
    }

    /// v0.20.1: render the cached Health-tab doctor checks to a plain-text
    /// snapshot and copy to clipboard. Uses `app.checks` only (no network).
    fn copy_health_report_to_clipboard(&mut self) {
        if self.checks.is_empty() {
            self.message = Some("no Health checks loaded yet — press `r` to refresh".into());
            return;
        }
        let mut out = String::new();
        out.push_str(&format!("AI Bridge {}\n", aibridge_core::VERSION_FULL));
        out.push_str(&format!(
            "Platform: {}\n\n",
            aibridge_platform::platform_name()
        ));
        for c in &self.checks {
            let tag = match c.status {
                aibridge_core::doctor::Status::Pass => "[ok  ]",
                aibridge_core::doctor::Status::Warn => "[warn]",
                aibridge_core::doctor::Status::Fail => "[fail]",
            };
            let detail = if c.detail.is_empty() {
                String::new()
            } else {
                format!(" — {}", c.detail)
            };
            out.push_str(&format!("{tag} {}{detail}\n", c.name));
        }
        match self.clipboard.copy(&out) {
            Ok(n) => self.message = Some(format!("copied {n} bytes to clipboard")),
            Err(e) => self.message = Some(format!("clipboard write failed: {e}")),
        }
    }

    fn handle_update_action(&mut self) {
        let idx = self.update_sel;
        if idx == 0 {
            // v0.25.0: dispatch on the TYPED self-update state. `Newer` STAGES the
            // update in-TUI (no shell drop); a detached helper applies it once all
            // aibridge processes exit. Every other state shows a footer + stays put.
            match &self.self_update_state {
                SelfUpdateState::Newer {
                    from, to, planned, ..
                } => {
                    if self.self_stage_rx.is_some() {
                        self.message = Some("update is already staging — please wait".into());
                    } else {
                        let rx = (self.self_stage_starter)(planned.clone());
                        self.self_stage_rx = Some(rx);
                        self.update_line = format!("Staging update {from} → {to}…");
                        // v0.28: platform-accurate copy — macOS applies immediately, others
                        // apply once all aibridge processes exit (see production_self_stage_starter).
                        #[cfg(target_os = "macos")]
                        {
                            self.message = Some(
                                "applying update now — reload Claude Code (or reopen the TUI) \
                                 to run the new version"
                                    .into(),
                            );
                        }
                        #[cfg(not(target_os = "macos"))]
                        {
                            self.message = Some(
                                "staging update — it applies automatically when all aibridge \
                                 processes (incl. Claude Code MCP servers) exit; this dashboard \
                                 keeps the current version until then"
                                    .into(),
                            );
                        }
                    }
                }
                SelfUpdateState::UpToDate { reason } => {
                    self.message = Some(format!("aibridge is up to date — {reason}"));
                }
                SelfUpdateState::Checking => {
                    self.message = Some(
                        "still checking GitHub for a newer release — try `u` again when complete"
                            .into(),
                    );
                }
                SelfUpdateState::Unprobed => {
                    // Kick a probe NOW and tell the user to retry.
                    self.start_self_update_probe();
                    self.message = Some(
                        "probing GitHub for a newer release — try `u` again when complete".into(),
                    );
                }
                SelfUpdateState::Error { detail } => {
                    self.message = Some(format!(
                        "self-update check failed: {detail} — press `c` to retry"
                    ));
                }
            }
            return;
        }
        let cli_idx = idx - 1;
        if cli_idx < self.cli_checks.len() {
            let c = &self.cli_checks[cli_idx];
            // v0.26.0: gate on the typed status BEFORE any mutation path, so an
            // installed-but-version-unknown tool can never auto-update (and is never
            // mislabeled "up to date"). NotInstalled/Outdated fall through to the
            // existing install/update logic.
            use aibridge_core::cli_update::CliStatus;
            match c.status() {
                CliStatus::UpToDate => {
                    self.message = Some(format!("{} is up to date — nothing to do.", c.tool));
                    return;
                }
                CliStatus::VersionUnknown => {
                    self.message = Some(format!(
                        "{}: installed but version unknown — not auto-updating \
                         (re-check after fixing its --version).",
                        c.tool
                    ));
                    return;
                }
                CliStatus::NotInstalled | CliStatus::Outdated => {}
            }
            // v0.20.0 R7 B3: use `safe_to_auto_run` so the trusted internal
            // `aibridge rtk install/update --yes` form is also accepted (and a
            // stale-PATH "aibridge" path is rejected).
            if c.suggested_command.is_none() || !c.safe_to_auto_run() {
                // Manual-only / unknown — show the hint, no mutation.
                let lead = if c.not_installed() {
                    "not installed"
                } else {
                    "manual update"
                };
                self.message = Some(format!(
                    "{}: {lead} — {}",
                    c.tool,
                    c.manual_note.as_deref().unwrap_or("see docs")
                ));
                return;
            }
            // v0.22.0: single in-flight constraint — block second `u` until the
            // current in-TUI worker finishes (events drained → Done observed).
            if self
                .active_cli_run
                .as_ref()
                .is_some_and(|r| r.finished.is_none())
            {
                self.message = Some("update in progress; please wait until it completes".into());
                return;
            }
            // v0.22.0: rtk-native paths get a DEDICATED in-TUI worker (it does the
            // rtk download/verify/atomic install with progress). rtk-via-brew falls
            // through to the generic streaming path below.
            use aibridge_core::cli_update::{InstallSource, RtkNativeAction};
            if c.tool == "rtk" {
                let action = match (&c.source, c.installable) {
                    (InstallSource::NativeInstaller { .. }, _) => Some(RtkNativeAction::Update),
                    (InstallSource::Unknown { .. }, true) => Some(RtkNativeAction::Install),
                    _ => None, // Brew or other → generic streaming path
                };
                if let Some(action) = action {
                    if self
                        .last_cli_run_result
                        .as_ref()
                        .is_some_and(|r| r.tool == "rtk")
                    {
                        self.last_cli_run_result = None;
                    }
                    let (rx, handle) = (self.cli_update_starter)(action);
                    self.active_cli_run = Some(ActiveCliRun {
                        tool: "rtk",
                        rx,
                        latest_stage: "starting…".to_string(),
                        started_at: Instant::now(),
                        finished: None,
                        handle: Some(handle),
                    });
                    self.message = Some("rtk update started — staying in TUI".into());
                    return;
                }
            }
            // v0.25.0: ALL other safe CLI updates stream IN-TUI (no shell drop). A
            // FreshInstall (first global install) requires a 2-key confirm; verified
            // updates start on a single `u`.
            let Some(argv) = c.suggested_command.clone() else {
                self.message = Some(format!("{}: no update command available", c.tool));
                return;
            };
            let tool = c.tool;
            let is_fresh = matches!(c.source, InstallSource::FreshInstall { .. });
            if is_fresh && self.fresh_install_armed != Some(tool) {
                // First press on a FreshInstall row → arm; do not run yet.
                self.fresh_install_armed = Some(tool);
                self.message = Some(format!(
                    "{tool}: first install — press `u` again to run `{}`",
                    argv.join(" ")
                ));
                return;
            }
            // Either a verified update (single press) or the armed FreshInstall's
            // second press → start the in-TUI stream.
            self.fresh_install_armed = None;
            if self
                .last_cli_run_result
                .as_ref()
                .is_some_and(|r| r.tool == tool)
            {
                self.last_cli_run_result = None;
            }
            let (rx, handle) = (self.cli_stream_starter)(tool, argv);
            self.active_cli_run = Some(ActiveCliRun {
                tool,
                rx,
                latest_stage: "starting…".to_string(),
                started_at: Instant::now(),
                finished: None,
                handle: Some(handle),
            });
            self.message = Some(format!("{tool} update started — staying in TUI"));
            return;
        }
        // Otherwise it's an MCP-pin row — read-only.
        self.message = Some(
            "MCP version pins are read-only in this view; edit ~/.claude.json or .mcp.json to change".into(),
        );
    }

    /// v0.27.0: `b` on the Update tab — the macOS Homebrew Terminal-handoff. Only
    /// actionable on macOS when codex/claude are missing and no package manager was
    /// found (`can_offer_homebrew_install_ui`); 2-key confirm (first `b` arms, second
    /// `b` opens Terminal). The sudo password is entered in Apple's Terminal — AI
    /// Bridge never sees it. `is_macos` is injected so the gate is unit-testable.
    fn handle_homebrew_install(&mut self, is_macos: bool) {
        if !aibridge_core::cli_update::can_offer_homebrew_install_ui(is_macos, &self.cli_checks) {
            self.brew_install_armed = false;
            self.message = Some(
                "Homebrew install is offered only on macOS when codex/claude are missing \
                 and neither Homebrew nor npm is found."
                    .into(),
            );
            return;
        }
        if !self.brew_install_armed {
            self.brew_install_armed = true;
            self.message = Some(
                "Press `b` again to open macOS Terminal and install Homebrew. You'll enter \
                 your Mac password in Terminal — AI Bridge never sees it."
                    .into(),
            );
            return;
        }
        // Second press → hand off to Terminal.
        self.brew_install_armed = false;
        match (self.homebrew_opener)() {
            Ok(()) => {
                self.message = Some(
                    "Opened macOS Terminal to install Homebrew. Finish it there (enter your \
                     password), then come back and press `r` to re-check."
                        .into(),
                );
            }
            Err(e) => {
                self.message = Some(format!(
                    "could not open Terminal for the Homebrew install: {e}"
                ));
            }
        }
    }

    /// v0.25.0: cancel a Waiting/Staged self-update (`x` on the Update tab).
    fn cancel_staged_update(&mut self) {
        match aibridge_core::staged_update::cancel_pending() {
            Ok(()) => {
                self.staged_status = aibridge_core::staged_update::read_update_status();
                self.message = Some("staged update cancelled.".into());
            }
            Err(e) => self.message = Some(format!("cancel: {e}")),
        }
    }

    /// v0.25.0: retry a Failed self-update (`g` on the Update tab).
    fn retry_staged_update(&mut self) {
        match aibridge_core::staged_update::retry_failed_update() {
            Ok(()) => {
                self.staged_status = aibridge_core::staged_update::read_update_status();
                self.message = Some("retrying staged update…".into());
            }
            Err(e) => self.message = Some(format!("retry: {e}")),
        }
    }

    /// Full refresh (startup + `r`): runs the doctor checks (no network), reloads the
    /// review status, and re-reads the MCP policies (codex + claude). Not called on
    /// the fast input tick.
    fn refresh_all(&mut self) {
        // v0.25.0: sweep crashed staged-update waiters + stale dirs, then cache status.
        aibridge_core::staged_update::sweep_stale_staged();
        self.staged_status = aibridge_core::staged_update::read_update_status();
        self.checks = doctor::run(std::path::Path::new(&self.cwd), false, false).checks;
        self.refresh_review();
        self.refresh_mcp();
        self.refresh_claude();
        if self.mcp_view == McpView::Tools {
            self.load_tool_states();
        }
    }

    /// Cheap (file read) — safe to call on the ~1s auto-refresh. Deliberately does NOT
    /// touch the review-model picker (rows/current/selection) so the auto-tick never
    /// fights the user's navigation or typing; that state reloads on launch + `r` only.
    fn refresh_review(&mut self) {
        self.review = progress::read_status(&self.cwd);
        self.review_summary = progress::status_report(&self.cwd);
    }

    /// O1c: reload the review-model picker from disk — the live model cache + the
    /// persisted config — and clamp the selection to the (possibly shorter) list. Pure
    /// reads (never writes the config). Called on launch + `r`, NOT on the 1s tick.
    fn refresh_review_models(&mut self) {
        self.model_rows = codex_models::list_models();
        self.model_current = review_mcp::codex_config();
        let len = model_choices(&self.model_rows).len();
        if self.model_sel >= len {
            self.model_sel = len.saturating_sub(1);
        }
    }

    /// O1c (pure over already-loaded fields; no IO): move the highlight onto the row the
    /// on-disk config points to. Used on launch + after a save so the picker re-centers
    /// on the configured model; plain `r` skips this to preserve navigation.
    fn realign_model_sel(&mut self) {
        self.model_sel = configured_choice_index(&self.model_rows, &self.model_current);
    }

    /// O1c: the picker row Enter acts on (None when the list is somehow empty).
    fn selected_model_choice(&self) -> Option<ModelChoice> {
        model_choices(&self.model_rows)
            .into_iter()
            .nth(self.model_sel)
    }

    /// O1c: Enter in list mode. Default/Model persist immediately (ctx PRESERVED for
    /// Model — only Default clears it); Custom opens the text editor pre-filled with the
    /// current custom slug when one is configured.
    fn apply_selected_model(&mut self) {
        let Some(choice) = self.selected_model_choice() else {
            return;
        };
        // Read the context-window override FRESH (not the cached self.model_current, which
        // only reloads on launch/`r`) so a ctx changed outside the TUI is never written
        // back stale. The TUI preserves whatever ctx is on disk; only Default clears it.
        let ctx = review_mcp::codex_config().model_context_window;
        match model_choice_set_args(&choice, ctx) {
            Some((model, ctx)) => {
                let is_clear = model.is_none();
                match review_mcp::set_codex_model(model.clone(), ctx) {
                    Ok(()) => {
                        self.refresh_review_models();
                        self.realign_model_sel();
                        self.message = Some(if is_clear {
                            "Cleared the review-model override (model + context). Restart Claude \
                             Code (or the AI Bridge MCP server) to apply."
                                .to_string()
                        } else {
                            format!(
                                "Configured review model = '{}'. Restart Claude Code (or the AI \
                                 Bridge MCP server) to apply — the running server keeps its current \
                                 review model until then.",
                                model.unwrap_or_default()
                            )
                        });
                    }
                    Err(e) => self.message = Some(format!("Couldn't save review model: {e}")),
                }
            }
            None => {
                // Custom row → enter the text editor, pre-filled when a custom slug is set.
                self.model_input = Some(self.custom_slug_if_configured());
            }
        }
    }

    /// O1c: the configured model slug IFF it isn't one of the cached rows (i.e. a custom
    /// slug) — used to pre-fill the editor so Enter on the Custom row never starts blank
    /// over an existing custom model.
    fn custom_slug_if_configured(&self) -> String {
        match &self.model_current.model {
            Some(slug) if !self.model_rows.iter().any(|m| &m.slug == slug) => slug.clone(),
            _ => String::new(),
        }
    }

    /// O1c: submit a typed custom slug. Preserves the context-window override. On success
    /// leaves input mode + re-centers; on EMPTY or FAILED input it keeps the editor open
    /// (restoring the typed slug) so the user can fix it or Esc. `handle_key` already
    /// `take()`s `model_input` before calling this, so this method owns putting it back.
    fn submit_custom_model(&mut self, slug: String) {
        if slug.is_empty() {
            self.model_input = Some(String::new());
            self.message = Some("Enter a model id, or press Esc to cancel.".to_string());
            return;
        }
        // Fresh read (see apply_selected_model): never overwrite a ctx changed outside.
        let ctx = review_mcp::codex_config().model_context_window;
        match review_mcp::set_codex_model(Some(slug.clone()), ctx) {
            Ok(()) => {
                self.model_input = None;
                self.refresh_review_models();
                self.realign_model_sel();
                self.message = Some(format!(
                    "Configured review model = '{slug}'. Restart Claude Code (or the AI Bridge MCP \
                     server) to apply — the running server keeps its current review model until then."
                ));
            }
            Err(e) => {
                self.model_input = Some(slug);
                self.message = Some(format!("Invalid model id: {e}"));
            }
        }
    }

    fn refresh_mcp(&mut self) {
        // Authoritative: ask codex (`codex mcp list --json`) so the list is correct
        // cross-platform + sees project/profile servers; `None` ⇒ codex unavailable.
        let allow = review_mcp::allowlist();
        self.mcp = match review_mcp::codex_inventory(&self.cwd) {
            review_mcp::Inventory::Available(servers) => Some(
                servers
                    .into_iter()
                    .map(|s| {
                        let enabled = allow.iter().any(|a| a == &s.name);
                        let interactive = review_mcp::server_looks_interactive(&s.name);
                        McpRow {
                            name: s.name,
                            enabled,
                            interactive,
                        }
                    })
                    .collect(),
            ),
            review_mcp::Inventory::Unavailable(_) => None,
        };
        // Clamp the selection to the (possibly shorter) list.
        if let Some(rows) = &self.mcp {
            if self.mcp_sel >= rows.len() {
                self.mcp_sel = rows.len().saturating_sub(1);
            }
        }
    }

    // --- pure navigation (unit-tested) ---
    fn next_tab(&mut self) {
        self.fresh_install_armed = None; // leaving the row cancels a pending fresh-install confirm
        self.tab = Tab::ALL[(self.tab.index() + 1) % Tab::ALL.len()];
    }
    fn prev_tab(&mut self) {
        self.fresh_install_armed = None;
        self.tab = Tab::ALL[(self.tab.index() + Tab::ALL.len() - 1) % Tab::ALL.len()];
    }
    fn move_down(&mut self) {
        self.fresh_install_armed = None; // changing the selected row cancels a pending confirm
        match self.tab {
            Tab::Mcp => match self.mcp_view {
                McpView::Servers => {
                    if let Some(rows) = &self.mcp {
                        if self.mcp_sel + 1 < rows.len() {
                            self.mcp_sel += 1;
                        }
                    }
                }
                McpView::Tools => {
                    if self.tool_sel + 1 < self.tool_rows.len() {
                        self.tool_sel += 1;
                    }
                }
            },
            Tab::ClaudeMcpInspector => match self.claude_view {
                McpView::Servers => {
                    if let Some(rows) = &self.claude_inventory {
                        if self.claude_sel + 1 < rows.len() {
                            self.claude_sel += 1;
                        }
                    }
                }
                McpView::Tools => {
                    if self.claude_tool_sel + 1 < self.claude_tool_rows.len() {
                        self.claude_tool_sel += 1;
                    }
                }
            },
            Tab::Health => self.health_scroll = self.health_scroll.saturating_add(1),
            Tab::Debug => self.debug_scroll = self.debug_scroll.saturating_add(1),
            Tab::Skills => {
                if !self.managed_rows.is_empty() && self.managed_sel + 1 < self.managed_rows.len() {
                    self.managed_sel += 1;
                }
            }
            Tab::Update => {
                self.update_sel = self.update_sel.saturating_add(1);
                self.clamp_update_sel();
            }
            // O1c: navigate the review-model picker (no-op while typing a custom slug).
            Tab::Review => {
                if self.model_input.is_none() {
                    let len = model_choices(&self.model_rows).len();
                    if self.model_sel + 1 < len {
                        self.model_sel += 1;
                    }
                }
            }
        }
    }
    fn move_up(&mut self) {
        self.fresh_install_armed = None;
        match self.tab {
            Tab::Mcp => match self.mcp_view {
                McpView::Servers => self.mcp_sel = self.mcp_sel.saturating_sub(1),
                McpView::Tools => self.tool_sel = self.tool_sel.saturating_sub(1),
            },
            Tab::ClaudeMcpInspector => match self.claude_view {
                McpView::Servers => self.claude_sel = self.claude_sel.saturating_sub(1),
                McpView::Tools => self.claude_tool_sel = self.claude_tool_sel.saturating_sub(1),
            },
            Tab::Health => self.health_scroll = self.health_scroll.saturating_sub(1),
            Tab::Debug => self.debug_scroll = self.debug_scroll.saturating_sub(1),
            Tab::Skills => self.managed_sel = self.managed_sel.saturating_sub(1),
            Tab::Update => self.update_sel = self.update_sel.saturating_sub(1),
            // O1c: navigate the review-model picker (no-op while typing a custom slug).
            Tab::Review => {
                if self.model_input.is_none() {
                    self.model_sel = self.model_sel.saturating_sub(1);
                }
            }
        }
    }
    fn selected_mcp(&self) -> Option<(&str, bool)> {
        self.mcp
            .as_ref()
            .and_then(|rows| rows.get(self.mcp_sel))
            .map(|r| (r.name.as_str(), r.enabled))
    }

    /// Toggle the selected MCP server's review-enabled state. Keeps the old state +
    /// shows the error inline if the write fails; precise wording about WHEN it applies.
    fn toggle_selected_mcp(&mut self) {
        if self.tab != Tab::Mcp {
            return;
        }
        let Some((name, was_on)) = self.selected_mcp().map(|(n, e)| (n.to_string(), e)) else {
            if self.mcp.is_none() {
                self.message =
                    Some("Codex config can't be read/parsed — fix it before toggling.".to_string());
            }
            return;
        };
        match review_mcp::set_enabled(&name, !was_on) {
            Ok(()) => {
                self.refresh_mcp();
                self.message = Some(format!(
                    "Saved: '{name}' = {}. Applies to the NEXT review child spawn (reload the \
                     Claude window to start one); a review already running keeps its policy.",
                    if was_on { "off" } else { "on" }
                ));
            }
            Err(e) => self.message = Some(format!("Couldn't save '{name}': {e}")),
        }
    }

    /// Enter the selected server's per-tool view (loads its CACHED tools — no launch).
    fn open_selected_server(&mut self) {
        if self.tab != Tab::Mcp || self.mcp_view != McpView::Servers {
            return;
        }
        let Some(name) = self.selected_mcp().map(|(n, _)| n.to_string()) else {
            return;
        };
        self.mcp_server = Some(name);
        self.mcp_view = McpView::Tools;
        self.tool_sel = 0;
        self.load_tool_states();
    }

    /// Reload the open server's tool rows from the cache (no launch).
    fn load_tool_states(&mut self) {
        self.tool_rows.clear();
        self.tools_fresh = false;
        self.tools_known = false;
        if let Some(server) = &self.mcp_server {
            if let Some(ct) = review_mcp::cached_tool_states(server) {
                self.tool_rows = ct.tools;
                self.tools_fresh = ct.fresh;
                self.tools_known = true;
            }
        }
        if self.tool_sel >= self.tool_rows.len() {
            self.tool_sel = self.tool_rows.len().saturating_sub(1);
        }
    }

    /// Esc: leave the per-tool view back to the server list. Returns whether handled
    /// (so the top-level Esc only quits when NOT in the tool view).
    fn mcp_back(&mut self) -> bool {
        if self.tab == Tab::Mcp && self.mcp_view == McpView::Tools {
            self.mcp_view = McpView::Servers;
            self.mcp_server = None;
            self.tool_rows.clear();
            true
        } else {
            false
        }
    }

    /// 'd': discover the open server's tools on a BACKGROUND thread (launches it —
    /// tools/list only). Idempotent while one is running.
    fn start_discover(&mut self) {
        if self.tab != Tab::Mcp || self.mcp_view != McpView::Tools || self.discovering.is_some() {
            return;
        }
        let Some(server) = self.mcp_server.clone() else {
            return;
        };
        let (tx, rx) = std::sync::mpsc::channel();
        self.discover_rx = Some(rx);
        self.discovering = Some(server.clone());
        self.message = Some(format!(
            "Discovering '{server}' tools (launching it briefly)..."
        ));
        std::thread::spawn(move || {
            let r = review_mcp::discover_server(&server);
            let _ = tx.send((server, r));
        });
    }

    fn poll_discover(&mut self) {
        let Some(rx) = &self.discover_rx else {
            return;
        };
        if let Ok((server, res)) = rx.try_recv() {
            self.discover_rx = None;
            self.discovering = None;
            // Server-scoped: if the user navigated to a DIFFERENT server meanwhile, a
            // late result must not clobber their view or message (it's cached anyway,
            // so it shows when they return). Only the matching server updates the UI.
            if self.mcp_server.as_deref() == Some(server.as_str()) {
                self.message = Some(match &res {
                    Ok(t) => format!("Discovered {} tool(s) of '{server}'.", t.len()),
                    Err(e) => format!("Discovery of '{server}' failed: {e}"),
                });
                self.load_tool_states();
            }
        }
    }

    /// Space/Enter in the per-tool view: toggle the selected tool (cache-based, no
    /// relaunch). Keeps the list + shows the outcome/err inline.
    fn toggle_selected_tool(&mut self) {
        let Some(server) = self.mcp_server.clone() else {
            return;
        };
        let Some((tool, on)) = self
            .tool_rows
            .get(self.tool_sel)
            .map(|(t, e)| (t.clone(), *e))
        else {
            return;
        };
        match review_mcp::toggle_tool_cached(&server, &tool, !on) {
            Ok(m) => {
                self.load_tool_states();
                self.message = Some(format!(
                    "Saved: {m}. Applies to the NEXT review child spawn (reload to start one)."
                ));
            }
            Err(e) => self.message = Some(format!("Couldn't toggle '{tool}': {e}")),
        }
    }

    /// 'a' / 'n' in the per-tool view: enable ALL of the open server's tools, or
    /// disable all of them (cache-based; 'n' needs a fresh discovery).
    fn set_all_tools_in_view(&mut self, on: bool) {
        let Some(server) = self.mcp_server.clone() else {
            return;
        };
        match review_mcp::set_all_tools(&server, on) {
            Ok(m) => {
                self.load_tool_states();
                self.message = Some(format!(
                    "Saved: {m}. Applies to the NEXT review child spawn (reload to start one)."
                ));
            }
            Err(e) => self.message = Some(format!("Couldn't set all tools: {e}")),
        }
    }
}

/// Entry point for `aibridge tui`. Refuses (with a hint) without an interactive
/// terminal so scripts/pipes never hang; otherwise runs the dashboard, always
/// restoring the terminal afterwards (ratatui also installs a panic hook on init).
pub fn run() -> Result<()> {
    if !std::io::stdin().is_terminal()
        || !std::io::stdout().is_terminal()
        || std::env::var("TERM")
            .map(|t| t.eq_ignore_ascii_case("dumb"))
            .unwrap_or(false)
    {
        println!(
            "AI Bridge: the dashboard needs an interactive terminal. Use \
             `aibridge status --watch` (live text), `aibridge doctor`, or \
             `aibridge review-mcp list` instead."
        );
        return Ok(());
    }
    let cwd = std::env::current_dir()?.to_string_lossy().to_string();
    let mut terminal = ratatui::init();
    let mut app = App::new(cwd);
    let res = run_loop(&mut terminal, &mut app);
    ratatui::restore();
    // v0.25.0: nothing runs after the TUI exits anymore. The aibridge SELF-update
    // STAGES in-TUI + a detached helper applies it (see `staged_update`); codex /
    // claude / brew / npm / rtk updates STREAM in-TUI via `cli_stream_starter` /
    // `cli_update_starter`. No shell drop, no after-exit prompts.
    res
}

fn run_loop(terminal: &mut DefaultTerminal, app: &mut App) -> Result<()> {
    let mut last_refresh = Instant::now();
    loop {
        terminal.draw(|f| ui(f, app))?;
        if event::poll(Duration::from_millis(250))? {
            if let Event::Key(k) = event::read()? {
                if k.kind == KeyEventKind::Press {
                    handle_key(app, k.code);
                }
            }
        }
        if app.quit {
            break;
        }
        app.poll_update(); // fold in a finished background update-check
        app.poll_discover(); // fold in a finished background tool-discovery
        app.poll_claude_discover(); // fold in a finished Claude-side discovery
        app.poll_managed_apply(); // fold in a finished background managed-skills apply
        app.poll_check_upstream(); // fold in the upstream probe result
        app.poll_bump(); // fold in a finished bump prepare/commit
        app.poll_debug_build(); // fold in a finished Debug-report build
        if app.tab == Tab::Skills && app.skills_report.is_none() {
            app.refresh_skills(); // lazy first compute (folder digests) on first view
        }
        if app.tab == Tab::Debug && app.debug_text.is_none() && app.debug_rx.is_none() {
            app.start_debug_build(); // lazy first build on first view
        }
        if app.tab == Tab::Update && app.cli_checks.is_empty() && app.cli_checks_rx.is_none() {
            app.start_cli_checks(); // lazy first run on first view
        }
        if app.tab == Tab::Update && !app.update_check_kicked && app.update_rx.is_none() {
            // v0.20.1 R3 B3: explicit one-shot guard so the lazy probe fires
            // exactly once per TUI session. Subsequent refreshes use `c`.
            app.update_check_kicked = true;
            app.start_self_update_probe();
        }
        // Live-refresh the (cheap) review status ~1s; heavy refresh only on `r`.
        if last_refresh.elapsed() >= Duration::from_secs(1) {
            app.refresh_review();
            last_refresh = Instant::now();
        }
    }
    Ok(())
}

fn handle_key(app: &mut App, code: KeyCode) {
    // O1c: while typing a custom review-model slug, the editor OWNS every key — routed
    // BEFORE the global q/Esc/Tab/arrow handlers so typing/Esc can't quit or switch tabs.
    // Guarded to the Review tab so it never leaks elsewhere.
    if app.tab == Tab::Review && app.model_input.is_some() {
        let mut buf = app.model_input.take().unwrap_or_default();
        match edit_model_input(&mut buf, code) {
            InputOutcome::Editing => app.model_input = Some(buf),
            InputOutcome::Submit(slug) => app.submit_custom_model(slug),
            InputOutcome::Cancel => app.model_input = None,
        }
        return;
    }
    // v0.25.0: any key other than `u` cancels a pending FreshInstall 2-key confirm —
    // so an intervening action (c/r/x/g/…) that may change the visible command forces
    // a fresh first press before a first global install can run.
    if !matches!(code, KeyCode::Char('u')) {
        app.fresh_install_armed = None;
    }
    // v0.27.0: any key other than `b` cancels a pending Homebrew-install 2-key confirm.
    if !matches!(code, KeyCode::Char('b')) {
        app.brew_install_armed = false;
    }
    let in_tools = app.tab == Tab::Mcp && app.mcp_view == McpView::Tools;
    // Any key that ISN'T the matching 2nd press of a Skills mutate-confirm cancels it.
    let is_skills_mutate = app.tab == Tab::Skills
        && (matches!(
            code,
            KeyCode::Char('s')
                | KeyCode::Char('m')
                | KeyCode::Char('i')
                | KeyCode::Char('n')
                | KeyCode::Char('d')
                | KeyCode::Char('x')
                | KeyCode::Char('p')
                | KeyCode::Char('o')
                | KeyCode::Char('M')
                | KeyCode::Char('U')
                | KeyCode::Char('B')
        ) || matches!(code, KeyCode::Enter));
    // Pressing any Skills key that ISN'T a continuation of the held bump preview cancels it
    // (so navigation/quit-key/etc. doesn't accidentally commit a preview).
    if app.tab == Tab::Skills && app.pending_bump.is_some() && !matches!(code, KeyCode::Char('B')) {
        app.clear_pending_bump();
    }
    if !is_skills_mutate {
        app.skills_confirm = None;
    }
    // CONCURRENCY GUARD: while a background `managed apply` is in flight, refuse every
    // mutating managed/personal-skill key. Without this guard, pressing `d`/`x` (which run
    // synchronously on the UI thread) would race the apply thread on the same lockfile +
    // mirrors. The core ProcessLock would catch a cross-process race, but a same-process
    // race needs a UI-level guard — keys aren't a concurrency primitive.
    if is_skills_mutate && app.managed_apply_rx.is_some() {
        app.message = Some(
            "A managed apply is already in progress — wait for it to finish, then try again."
                .into(),
        );
        return;
    }
    // v0.22.0: while an in-TUI CLI update is running, block `q` and `Esc`
    // (top-level quit) to prevent dropping the worker channel mid-flow. The
    // user's footer message says to wait. Note: crossterm raw mode delivers
    // Ctrl+C as a key event (NOT an OS signal) and v0.22.0 does NOT install a
    // Ctrl+C handler — closing the terminal window is the only escape hatch
    // for a stuck worker, with the partial-state caveats documented in
    // CHANGELOG.
    let cli_update_active = app
        .active_cli_run
        .as_ref()
        .is_some_and(|r| r.finished.is_none());
    match code {
        KeyCode::Char('q') => {
            if cli_update_active {
                app.message = Some("update in progress; please wait until it completes".into());
            } else {
                app.quit = true;
            }
        }
        // Esc backs out of the per-tool view first; only quits at the top level.
        // (Bind first so the arm body isn't a lone `if` — avoids clippy
        // collapsible_match wanting a side-effecting match guard.)
        KeyCode::Esc => {
            let backed_out = app.mcp_back() || app.claude_back();
            if !backed_out {
                if cli_update_active {
                    app.message = Some("update in progress; please wait until it completes".into());
                } else {
                    app.quit = true;
                }
            }
        }
        KeyCode::Tab | KeyCode::Right => app.next_tab(),
        KeyCode::BackTab | KeyCode::Left => app.prev_tab(),
        KeyCode::Down | KeyCode::Char('j') => app.move_down(),
        KeyCode::Up | KeyCode::Char('k') => app.move_up(),
        // Space toggles: a tool in the per-tool view, else the selected server.
        // Tab-guarded so the Claude Inspector's view-only Space arm below can fire.
        KeyCode::Char(' ') if in_tools => app.toggle_selected_tool(),
        KeyCode::Char(' ') if app.tab == Tab::Mcp => app.toggle_selected_mcp(),
        // Enter opens a server's tools (server list) or toggles a tool (tool view).
        KeyCode::Enter if app.tab == Tab::Mcp && app.mcp_view == McpView::Servers => {
            app.open_selected_server()
        }
        KeyCode::Enter if in_tools => app.toggle_selected_tool(),
        KeyCode::Char('a') if in_tools => app.set_all_tools_in_view(true),
        KeyCode::Char('n') if in_tools => app.set_all_tools_in_view(false),
        KeyCode::Char('d') if in_tools => app.start_discover(),
        // Claude MCP Inspector: VIEW-ONLY navigation. Enter opens a server's tool view;
        // mutating keys (Space anywhere, `a`/`n` in tools view) no-op with a "view-only"
        // footer so the user gets feedback instead of silence.
        KeyCode::Enter
            if app.tab == Tab::ClaudeMcpInspector && app.claude_view == McpView::Servers =>
        {
            app.open_selected_claude()
        }
        KeyCode::Char(' ') if app.tab == Tab::ClaudeMcpInspector => app.claude_view_only_message(),
        KeyCode::Enter
            if app.tab == Tab::ClaudeMcpInspector && app.claude_view == McpView::Tools =>
        {
            app.claude_view_only_message()
        }
        KeyCode::Char('a')
            if app.tab == Tab::ClaudeMcpInspector && app.claude_view == McpView::Tools =>
        {
            app.claude_view_only_message()
        }
        KeyCode::Char('n')
            if app.tab == Tab::ClaudeMcpInspector && app.claude_view == McpView::Tools =>
        {
            app.claude_view_only_message()
        }
        KeyCode::Char('d')
            if app.tab == Tab::ClaudeMcpInspector && app.claude_view == McpView::Tools =>
        {
            app.start_claude_discover()
        }
        KeyCode::Char('c') if app.tab == Tab::Update => {
            // v0.20.1 R3 B6: manual `c` reruns the probe UNLESS one is in flight.
            // Lazy auto-trigger is one-shot via `update_check_kicked`; `c` lets
            // the user explicitly refresh after an UpToDate/Error result.
            if app.update_rx.is_some() {
                app.message = Some("already checking; please wait".into());
            } else {
                app.start_self_update_probe();
            }
        }
        KeyCode::Char('u') if app.tab == Tab::Update => {
            // Per-row dispatch: self vs verified-pkg-manager CLI vs manual-only.
            // Self-update STAGES in-TUI (v0.25.0); CLI rows defer to after-exit.
            app.handle_update_action();
        }
        // v0.25.0: staged self-update controls.
        KeyCode::Char('x') if app.tab == Tab::Update => app.cancel_staged_update(),
        KeyCode::Char('g') if app.tab == Tab::Update => app.retry_staged_update(),
        // v0.27.0: macOS Homebrew Terminal-handoff (2-key confirm). Real macOS gate
        // is passed at the call site so the handler stays unit-testable.
        KeyCode::Char('b') if app.tab == Tab::Update => {
            app.handle_homebrew_install(cfg!(target_os = "macos"))
        }
        // Skills tab: sync (s) / migrate (m) MUTATE the filesystem → 2-key confirm.
        KeyCode::Char('s') if app.tab == Tab::Skills => {
            if app.skills_confirm_press('s') {
                app.apply_skills(false);
            } else {
                app.message = Some(
                    "Press s again to SYNC (add missing hub->agents); any other key cancels".into(),
                );
            }
        }
        KeyCode::Char('m') if app.tab == Tab::Skills => {
            if app.skills_confirm_press('m') {
                app.apply_skills(true);
            } else {
                app.message = Some(
                    "Press m again to MIGRATE (add missing codex-legacy->hub); any other key cancels".into(),
                );
            }
        }
        // Managed skills: 'i' = apply ALL (fetch pinned sources + mirror) → 2-key confirm.
        KeyCode::Char('i') if app.tab == Tab::Skills => {
            if app.skills_confirm_press('i') {
                app.start_managed_apply(aibridge_core::managed_skills::Target::All, false, false);
            } else {
                app.message = Some(
                    "Press i again to INSTALL/UPDATE all managed skills (fetches pinned sources); any other key cancels".into(),
                );
            }
        }
        // Managed skills: 'n' = init starter manifest → 2-key confirm.
        KeyCode::Char('n') if app.tab == Tab::Skills => {
            if app.skills_confirm_press('n') {
                app.managed_init();
            } else {
                app.message = Some(
                    "Press n again to write a starter manifest at ~/.ai-bridge/skills-managed.toml (nothing installs); any other key cancels".into(),
                );
            }
        }
        // O1c: Enter on the Review tab = configure the selected review model (list mode;
        // input mode is intercepted at the top of handle_key).
        KeyCode::Enter if app.tab == Tab::Review => app.apply_selected_model(),
        // Managed skills: Enter = INSTALL/UPDATE the selected skill → 2-key confirm.
        KeyCode::Enter if app.tab == Tab::Skills => {
            if app.selected_managed().is_none() {
                app.message = Some(
                    "No managed skill selected (the list is empty — press 'n' to init a manifest, then add entries)".into(),
                );
            } else if app.skills_confirm_press('\n') {
                if let Some(name) = app.selected_managed() {
                    app.start_managed_apply(
                        aibridge_core::managed_skills::Target::One(name),
                        false,
                        false,
                    );
                }
            } else if let Some(n) = app.selected_managed() {
                app.message = Some(format!(
                    "Press Enter again to INSTALL/UPDATE {n} (fetches pinned source); any other key cancels"
                ));
            }
        }
        // Managed skills: 'p' = apply selected with --repair (re-mirror a drifted owned).
        KeyCode::Char('p') if app.tab == Tab::Skills => {
            if app.selected_managed().is_none() {
                app.skills_confirm = None;
            } else if app.skills_confirm_press('p') {
                if let Some(name) = app.selected_managed() {
                    app.start_managed_apply(
                        aibridge_core::managed_skills::Target::One(name),
                        true,
                        false,
                    );
                }
            } else if let Some(n) = app.selected_managed() {
                app.message = Some(format!(
                    "Press p again to REPAIR {n} (re-mirror an owned-but-hand-edited copy, discarding edits); any other key cancels"
                ));
            }
        }
        // Managed skills: 'o' = apply selected with --adopt (take over an identical foreign).
        KeyCode::Char('o') if app.tab == Tab::Skills => {
            if app.selected_managed().is_none() {
                app.skills_confirm = None;
            } else if app.skills_confirm_press('o') {
                if let Some(name) = app.selected_managed() {
                    app.start_managed_apply(
                        aibridge_core::managed_skills::Target::One(name),
                        false,
                        true,
                    );
                }
            } else if let Some(n) = app.selected_managed() {
                app.message = Some(format!(
                    "Press o again to ADOPT {n} (take over an existing byte-identical foreign folder); any other key cancels"
                ));
            }
        }
        // Managed skills: 'd' = disable selected (remove owned mirrors; keep source+lock).
        KeyCode::Char('d') if app.tab == Tab::Skills => {
            if app.selected_managed().is_none() {
                app.skills_confirm = None;
            } else if app.skills_confirm_press('d') {
                app.managed_disable_selected();
            } else if let Some(n) = app.selected_managed() {
                app.message = Some(format!(
                    "Press d again to DISABLE {n} (remove Bridge mirrors from both CLI dirs; source+lock kept); any other key cancels"
                ));
            }
        }
        // Managed skills: 'x' = remove selected (mirrors + source + lock entry; ownership-checked).
        KeyCode::Char('x') if app.tab == Tab::Skills => {
            if app.selected_managed().is_none() {
                app.skills_confirm = None;
            } else if app.skills_confirm_press('x') {
                app.managed_remove_selected();
            } else if let Some(n) = app.selected_managed() {
                app.message = Some(format!(
                    "Press x again to REMOVE {n} (delete owned mirrors + source + lock entry); any other key cancels"
                ));
            }
        }
        // `M` = migrate-and-install the selected skill (quarantine foreign → ~/.ai-bridge/
        // backups/ then apply). Background thread; 2-key confirm.
        KeyCode::Char('M') if app.tab == Tab::Skills => {
            if app.selected_managed().is_none() {
                app.skills_confirm = None;
            } else if app.skills_confirm_press('M') {
                app.start_managed_migrate();
            } else if let Some(n) = app.selected_managed() {
                app.message = Some(format!(
                    "Press M again to MIGRATE-AND-INSTALL {n} (foreign mirror is moved to ~/.ai-bridge/backups/ then apply runs); any other key cancels"
                ));
            }
        }
        // `U` = check upstream for every tracked skill (network on a background thread).
        // Not state-changing → no 2-key confirm needed; single press is enough.
        KeyCode::Char('U') if app.tab == Tab::Skills => {
            app.skills_confirm = None;
            app.start_check_upstream();
        }
        // `B` = bump-and-apply: FIRST press stages the upstream candidate (preview); SECOND
        // press commits (writes manifest + applies). Preview drops if any other key is
        // pressed (handled near the top of `handle_key`).
        KeyCode::Char('B') if app.tab == Tab::Skills => {
            if app.pending_bump.is_some() {
                app.start_bump_commit();
            } else {
                app.start_bump_prepare();
            }
        }
        KeyCode::Char('r') => {
            app.refresh_all();
            if app.tab == Tab::Review {
                // O1c: reload the live model cache + persisted config. Preserve the
                // current navigation position (don't realign) — manual refresh only.
                app.refresh_review_models();
            }
            if app.tab == Tab::Skills {
                app.refresh_skills();
            }
            if app.tab == Tab::Debug {
                // Force a rebuild — the previous text is replaced with the building
                // notice and a fresh worker is kicked off (idempotent if one's in flight).
                app.debug_text = None;
                app.start_debug_build();
            }
            if app.tab == Tab::Update {
                // Re-run the CLI-update checks (codex/claude/rtk + MCP pins).
                app.cli_checks.clear();
                app.start_cli_checks();
            }
        }
        // v0.20.1: `y` copies the current tab's report to the clipboard.
        // Debug + Health are supported.
        KeyCode::Char('y') if app.tab == Tab::Debug => {
            app.copy_debug_report_to_clipboard();
        }
        KeyCode::Char('y') if app.tab == Tab::Health => {
            app.copy_health_report_to_clipboard();
        }
        _ => {}
    }
}

fn ui(f: &mut Frame, app: &App) {
    let rows = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(0),
        Constraint::Length(2),
    ])
    .split(f.area());

    let titles: Vec<Line> = Tab::ALL.iter().map(|t| Line::from(t.title())).collect();
    let tabs = Tabs::new(titles)
        .select(app.tab.index())
        .block(Block::default().borders(Borders::ALL).title("AI Bridge"))
        .highlight_style(Style::default().fg(Color::Black).bg(Color::Cyan));
    f.render_widget(tabs, rows[0]);

    match app.tab {
        Tab::Health => render_health(f, app, rows[1]),
        Tab::Review => render_review(f, app, rows[1]),
        Tab::ClaudeMcpInspector => render_claude_inspector(f, app, rows[1]),
        Tab::Mcp => render_mcp(f, app, rows[1]),
        Tab::Skills => render_skills(f, app, rows[1]),
        Tab::Update => render_update(f, app, rows[1]),
        Tab::Debug => render_debug(f, app, rows[1]),
    }

    let help = match app.tab {
        Tab::Mcp if app.mcp_view == McpView::Tools => {
            "Up/Down: tool | Space/Enter: toggle | a: all | n: none | d: discover | Esc: back | q: quit"
        }
        Tab::Mcp => {
            "Tab/Left/Right: tabs | Up/Down: server | Space: on/off | Enter: per-tool | r: refresh | q: quit"
        }
        Tab::ClaudeMcpInspector if app.claude_view == McpView::Tools => {
            "Up/Down: tool | d: discover | Esc: back | (view-only — toggles disabled) | q: quit"
        }
        Tab::ClaudeMcpInspector => {
            "Tab/Left/Right: tabs | Up/Down: server | Enter: view tools | r: refresh | (view-only — toggles via ~/.claude.json) | q: quit"
        }
        Tab::Health => {
            "Tab/Left/Right: tabs | Up/Down: scroll | y: copy all to clipboard | r: refresh | q: quit"
        }
        Tab::Skills => {
            "Up/Dn: select | Enter: install | M: migrate-and-install | U: check upstream | B: bump (preview→commit) | p: repair | o: adopt | d: disable | x: remove | i: apply all | n: init | s/m: personal sync/migrate | r: refresh | q: quit"
        }
        Tab::Review if app.model_input.is_some() => {
            "type model id | Enter: save | Esc: cancel"
        }
        Tab::Review => {
            "Tab/Left/Right: tabs | Up/Dn: select | Enter: configure model | r: refresh | q: quit"
        }
        Tab::Update => {
            "↑/↓: select | c: check self | u: update/stage | x: cancel staged | g: retry staged | r: re-check | q: quit"
        }
        Tab::Debug => {
            "Tab/Left/Right: tabs | Up/Down: scroll | y: copy all to clipboard | r: rebuild | q: quit"
        }
    };
    let footer = match &app.message {
        Some(m) => Line::from(Span::styled(m.clone(), Style::default().fg(Color::Yellow))),
        None => Line::from(Span::styled(help, Style::default().fg(Color::DarkGray))),
    };
    f.render_widget(Paragraph::new(footer).wrap(Wrap { trim: true }), rows[2]);
}

fn render_skills(f: &mut Frame, app: &App, area: Rect) {
    // Full-screen managed-skills list. The personal-skills doctor moved to the Health tab
    // (see `review feed (skills)`) — it was passive display, not actionable here.
    let manifest_hint = "manifest: ~/.ai-bridge/skills-managed.toml — edit externally, then 'r'";
    let items: Vec<ListItem> = if let Some(err) = &app.managed_error {
        vec![ListItem::new(format!(
            "(no managed skills — {err}. Press 'n' to init a starter manifest.)"
        ))]
    } else if app.managed_rows.is_empty() {
        vec![ListItem::new(
            "(no managed skills declared yet. Press 'n' to init a starter manifest, then edit it.)",
        )]
    } else {
        app.managed_rows
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let mark = if s.attention { "!" } else { " " };
                let en = if s.enabled { "on " } else { "off" };
                let prefix = if i == app.managed_sel { ">" } else { " " };
                // Annotate the state with an upstream candidate when one is known.
                let upstream = app
                    .upstream_candidates
                    .get(&s.name)
                    .and_then(|c| match (&c.upstream_sha, c.update_available()) {
                        (Some(new), true) => Some(format!(
                            "  ↑ upstream {} available",
                            &new[..new.len().min(8)]
                        )),
                        (None, _) => Some("  (upstream probe failed — press U)".into()),
                        _ => None,
                    })
                    .unwrap_or_default();
                let line = format!(
                    "{prefix} {mark} {:<22} [{en}] {:<10} {}{}",
                    s.name, s.pin, s.state, upstream
                );
                let style = if i == app.managed_sel {
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else if s.attention || upstream.contains("upstream") {
                    Style::default().fg(Color::Yellow)
                } else {
                    Style::default()
                };
                ListItem::new(line).style(style)
            })
            .collect()
    };
    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .title(format!("Managed skills  ({manifest_hint})")),
    );
    f.render_widget(list, area);
}

fn render_health(f: &mut Frame, app: &App, area: Rect) {
    let lines: Vec<Line> = app
        .checks
        .iter()
        .map(|c| {
            let (tag, color) = match c.status {
                Status::Pass => ("ok  ", Color::Green),
                Status::Warn => ("warn", Color::Yellow),
                Status::Fail => ("FAIL", Color::Red),
            };
            let mut spans = vec![
                Span::styled(format!("[{tag}] "), Style::default().fg(color)),
                Span::raw(c.name.clone()),
            ];
            if !c.detail.is_empty() {
                spans.push(Span::styled(
                    format!(" - {}", c.detail),
                    Style::default().fg(Color::Gray),
                ));
            }
            Line::from(spans)
        })
        .collect();
    let p = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Health  (aibridge doctor)")
                .title_top(Line::from("[Y] Copy entire report").right_aligned()),
        )
        .wrap(Wrap { trim: false })
        .scroll((app.health_scroll, 0));
    f.render_widget(p, area);
}

fn render_review(f: &mut Frame, app: &App, area: Rect) {
    let mut lines: Vec<Line> = vec![Line::from(format!("Project: {}", app.cwd)), Line::from("")];
    match &app.review {
        Some(v) => {
            let s = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("-").to_string();
            let n = |k: &str| v.get(k).and_then(|x| x.as_u64());
            let active = v.get("active").and_then(|x| x.as_bool()).unwrap_or(false);
            let state = if active { "IN PROGRESS" } else { "idle / done" };
            lines.push(Line::from(vec![
                Span::raw("State:   "),
                Span::styled(
                    state,
                    Style::default().fg(if active { Color::Cyan } else { Color::Green }),
                ),
            ]));
            lines.push(Line::from(format!("Phase:   {}", s("phase"))));
            lines.push(Line::from(format!(
                "Elapsed: {}s    Events: {}    Tokens: {}",
                n("elapsed_s").unwrap_or(0),
                n("events").unwrap_or(0),
                n("tokens")
                    .map(|t| t.to_string())
                    .unwrap_or_else(|| "-".into()),
            )));
            lines.push(Line::from(format!("Last event: {}", s("last_event"))));
        }
        None => lines.push(Line::from("No review status yet for this project.")),
    }
    if let Some(summary) = &app.review_summary {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            summary.clone(),
            Style::default().fg(Color::Yellow),
        )));
    }

    // ── O1c: review-model picker ──
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Review model",
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )));
    let cfg_model = app
        .model_current
        .model
        .clone()
        .unwrap_or_else(|| "default (codex config.toml)".to_string());
    let cfg_ctx = app
        .model_current
        .model_context_window
        .map(|n| n.to_string())
        .unwrap_or_else(|| "default".to_string());
    lines.push(Line::from(Span::styled(
        format!("Configured:  model={cfg_model}   context={cfg_ctx}"),
        Style::default().fg(Color::DarkGray),
    )));
    if app.model_rows.is_empty() {
        lines.push(Line::from(Span::styled(
            "(no live model cache found — pick Custom to type a model id)",
            Style::default().fg(Color::Yellow),
        )));
    }
    let choices = model_choices(&app.model_rows);
    let configured = configured_choice_index(&app.model_rows, &app.model_current);
    let custom_slug = app.custom_slug_if_configured();
    for (i, choice) in choices.iter().enumerate() {
        let label = match choice {
            ModelChoice::Default => "Default (use codex config.toml — no override)".to_string(),
            ModelChoice::Model(m) => {
                let ctx = m
                    .context_window
                    .map(|n| format!("   ctx {n}"))
                    .unwrap_or_default();
                if m.display_name == m.slug {
                    format!("{}{ctx}", m.slug)
                } else {
                    format!("{} ({}){ctx}", m.display_name, m.slug)
                }
            }
            ModelChoice::Custom => {
                if custom_slug.is_empty() {
                    "Custom model id…".to_string()
                } else {
                    format!("Custom: {custom_slug}")
                }
            }
        };
        let prefix = if i == app.model_sel { ">" } else { " " };
        let marker = if i == configured {
            "  ● configured"
        } else {
            ""
        };
        let style = if i == app.model_sel {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else if i == configured {
            Style::default().fg(Color::Green)
        } else {
            Style::default()
        };
        lines.push(Line::from(Span::styled(
            format!("{prefix} {label}{marker}"),
            style,
        )));
    }
    if let Some(buf) = &app.model_input {
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::raw("New model id: "),
            Span::styled(
                format!("{buf}_"),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
        ]));
        lines.push(Line::from(Span::styled(
            "Enter: save   Esc: cancel   (allowed: letters, digits, '.', '_', '-')",
            Style::default().fg(Color::DarkGray),
        )));
    }

    let p = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Live review  (aibridge status)"),
        )
        .wrap(Wrap { trim: true });
    f.render_widget(p, area);
}

fn render_mcp(f: &mut Frame, app: &App, area: Rect) {
    if app.mcp_view == McpView::Tools {
        render_mcp_tools(f, app, area);
        return;
    }
    let block = Block::default().borders(Borders::ALL).title(
        "Codex MCP servers DURING reviews  (Space: on/off; Enter: per-tool; default: all off)",
    );
    match &app.mcp {
        None => {
            let p = Paragraph::new(
                "Couldn't query codex's MCP inventory (`codex mcp list --json` failed — codex not \
                 on PATH, or it errored). Put codex on PATH and press 'r' to retry. (Reviews still \
                 enforce the policy from the config file.)",
            )
            .style(Style::default().fg(Color::Yellow))
            .wrap(Wrap { trim: true })
            .block(block);
            f.render_widget(p, area);
        }
        Some(rows) if rows.is_empty() => {
            let p = Paragraph::new("No codex MCP servers configured - reviews run tool-free.")
                .wrap(Wrap { trim: true })
                .block(block);
            f.render_widget(p, area);
        }
        Some(rows) => {
            let items: Vec<ListItem> = rows
                .iter()
                .enumerate()
                .map(|(i, r)| {
                    let mark = if r.enabled { "[x]" } else { "[ ]" };
                    let warn = if r.enabled && r.interactive {
                        "   (!) browser/scrape - can stall a review"
                    } else {
                        ""
                    };
                    let mut style = Style::default();
                    if i == app.mcp_sel {
                        style = style.add_modifier(Modifier::REVERSED);
                    } else if r.enabled && r.interactive {
                        style = style.fg(Color::Yellow);
                    }
                    ListItem::new(Line::from(format!("{mark} {}{warn}", r.name))).style(style)
                })
                .collect();
            f.render_widget(List::new(items).block(block), area);
        }
    }
}

fn render_mcp_tools(f: &mut Frame, app: &App, area: Rect) {
    let server = app.mcp_server.as_deref().unwrap_or("?");
    let block = Block::default().borders(Borders::ALL).title(format!(
        "Tools of '{server}' DURING reviews  (Space: toggle; a: all; n: none; d: discover; Esc: back)"
    ));
    if app.discovering.as_deref() == Some(server) {
        let p = Paragraph::new(format!(
            "Discovering '{server}' tools (launching it briefly)..."
        ))
        .wrap(Wrap { trim: true })
        .block(block);
        f.render_widget(p, area);
        return;
    }
    if !app.tools_known {
        let p = Paragraph::new(format!(
            "'{server}' tools haven't been discovered yet.\n\nPress 'd' to discover — this briefly \
             launches the server (tools/list only; never calls a tool, so it can't hang)."
        ))
        .wrap(Wrap { trim: true })
        .block(block);
        f.render_widget(p, area);
        return;
    }
    let mut items: Vec<ListItem> = Vec::new();
    if !app.tools_fresh {
        items.push(ListItem::new(Line::from(Span::styled(
            "(cache is STALE — the server's config changed; press 'd' to re-discover. \
             Until then 'some' mode is fail-closed = the whole server is off in reviews.)",
            Style::default().fg(Color::Yellow),
        ))));
    }
    if app.tool_rows.is_empty() {
        items.push(ListItem::new("(the server reported no tools)"));
    }
    for (i, (name, on)) in app.tool_rows.iter().enumerate() {
        let mark = if *on { "[x]" } else { "[ ]" };
        let style = if i == app.tool_sel {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default()
        };
        items.push(ListItem::new(Line::from(format!("{mark} {name}"))).style(style));
    }
    f.render_widget(List::new(items).block(block), area);
}

/// What the Inspector's top-level view should show. Pure (no ratatui types) so the
/// decision logic is unit-testable — the Codex Stop-gate F2 regression was that the
/// render path took its empty-state branch even when warnings should have been
/// surfaced instead. The unit test `claude_inspector_view_prefers_warnings_over_empty_state`
/// locks the precedence down.
#[derive(Debug, PartialEq, Eq)]
enum InspectorView {
    /// `Inventory::Unavailable(why)` — couldn't read any source.
    Error(String),
    /// Servers list is empty but at least one source had a parse/read error. Show
    /// the warnings INSTEAD of the cheerful "no servers configured" hint.
    WarningsOnly(Vec<String>),
    /// Nothing wrong, nothing to show — render the friendly empty-state hint.
    Empty,
    /// Some servers (+ possibly warnings).
    Servers { warnings: Vec<String> },
    /// We haven't actually called `refresh_claude` yet (initial App state); ask the
    /// user to press `r`.
    NotLoaded,
}

fn claude_inspector_view(
    inventory: Option<&[claude_mcp::ClaudeServer]>,
    error: Option<&str>,
    warnings: &[String],
) -> InspectorView {
    if let Some(e) = error {
        return InspectorView::Error(e.to_string());
    }
    let Some(rows) = inventory else {
        return InspectorView::NotLoaded;
    };
    if rows.is_empty() {
        if !warnings.is_empty() {
            return InspectorView::WarningsOnly(warnings.to_vec());
        }
        return InspectorView::Empty;
    }
    InspectorView::Servers {
        warnings: warnings.to_vec(),
    }
}

/// Claude MCP Inspector — view-only. Mirrors the Codex MCP server-list / tool-view
/// shape so muscle memory transfers; only DISCOVERY is functional (no toggling).
fn render_claude_inspector(f: &mut Frame, app: &App, area: Rect) {
    if app.claude_view == McpView::Tools {
        render_claude_inspector_tools(f, app, area);
        return;
    }
    let block = Block::default().borders(Borders::ALL).title(
        "Claude MCP Inspector  (Enter: view tools  ·  view-only: toggles via ~/.claude.json)",
    );
    let view = claude_inspector_view(
        app.claude_inventory.as_deref(),
        app.claude_inventory_error.as_deref(),
        &app.claude_warnings,
    );
    match view {
        InspectorView::Error(err) => {
            let p = Paragraph::new(err)
                .style(Style::default().fg(Color::Yellow))
                .wrap(Wrap { trim: true })
                .block(block);
            f.render_widget(p, area);
        }
        InspectorView::NotLoaded => {
            let p = Paragraph::new("Claude MCP inventory not loaded yet — press 'r' to refresh.")
                .style(Style::default().fg(Color::Yellow))
                .wrap(Wrap { trim: true })
                .block(block);
            f.render_widget(p, area);
        }
        InspectorView::WarningsOnly(warnings) => {
            let mut text = String::from(
                "Couldn't fully load Claude MCP configuration. \
                 The Inspector reached at least one source but the following had problems:\n\n",
            );
            for w in &warnings {
                text.push_str("  ! ");
                text.push_str(w);
                text.push('\n');
            }
            text.push_str(
                "\nFix the source above (or add `mcpServers` entries) and press 'r' to reload.",
            );
            let p = Paragraph::new(text)
                .style(Style::default().fg(Color::Yellow))
                .wrap(Wrap { trim: true })
                .block(block);
            f.render_widget(p, area);
        }
        InspectorView::Empty => {
            let p = Paragraph::new(
                "No Claude MCP servers configured. Add one with `claude mcp add -s user` or commit \
                 a project `.mcp.json`. Reload the dashboard with 'r' afterwards.",
            )
            .wrap(Wrap { trim: true })
            .block(block);
            f.render_widget(p, area);
        }
        InspectorView::Servers { warnings } => {
            // Build a single List with optional yellow warning rows on top, then a
            // separator, then the servers.
            let rows = app.claude_inventory.as_deref().unwrap_or(&[]);
            let mut items: Vec<ListItem> = Vec::new();
            for w in &warnings {
                items.push(
                    ListItem::new(Line::from(format!("  ! {w}")))
                        .style(Style::default().fg(Color::Yellow)),
                );
            }
            if !warnings.is_empty() {
                items.push(ListItem::new(Line::from("  ───────────────────────")));
            }
            for (i, s) in rows.iter().enumerate() {
                let scope = match &s.scope {
                    claude_mcp::ClaudeScope::User { .. } => "user",
                    claude_mcp::ClaudeScope::Project { .. } => "project",
                };
                let overrides = if s.overrides_user {
                    "  (overrides user)"
                } else {
                    ""
                };
                let line = format!(
                    "  {:<20} [{scope:<7}] [{transport}]{overrides}",
                    s.name,
                    transport = s.transport.label()
                );
                let style = if i == app.claude_sel {
                    Style::default().add_modifier(Modifier::REVERSED)
                } else if s.overrides_user {
                    Style::default().fg(Color::Yellow)
                } else {
                    Style::default()
                };
                items.push(ListItem::new(Line::from(line)).style(style));
            }
            f.render_widget(List::new(items).block(block), area);
        }
    }
}

fn render_claude_inspector_tools(f: &mut Frame, app: &App, area: Rect) {
    let server_name = app
        .claude_server
        .as_ref()
        .map(|s| s.name.clone())
        .unwrap_or_else(|| "?".into());
    let block = Block::default().borders(Borders::ALL).title(format!(
        "Tools of Claude server '{server_name}'  (d: discover · view-only · Esc: back)"
    ));
    if app.claude_discovering.as_deref() == Some(server_name.as_str()) {
        let p = Paragraph::new(format!(
            "Discovering '{server_name}' (Claude side) — launching it briefly..."
        ))
        .wrap(Wrap { trim: true })
        .block(block);
        f.render_widget(p, area);
        return;
    }
    // If the selected server isn't stdio, give the user a transport-aware panel so
    // they know WHY 'd' wouldn't work (parity with the Codex side's HTTP message).
    let stdio = matches!(
        app.claude_server.as_ref().map(|s| &s.transport),
        Some(claude_mcp::Transport::Stdio { .. })
    );
    if !stdio {
        let label = app
            .claude_server
            .as_ref()
            .map(|s| s.transport.label().to_string())
            .unwrap_or_else(|| "?".into());
        let p = Paragraph::new(format!(
            "'{server_name}' uses transport '{label}' — tool discovery requires stdio.\n\nTo toggle \
             this server in Claude, edit ~/.claude.json (or the project's .mcp.json) and restart \
             Claude. Management UX is a follow-up plan."
        ))
        .wrap(Wrap { trim: true })
        .block(block);
        f.render_widget(p, area);
        return;
    }
    if !app.claude_tools_known {
        let p = Paragraph::new(
            "Tools haven't been discovered yet.\n\nPress 'd' to discover — this briefly launches \
             the server (tools/list only; never calls a tool, so it can't hang).",
        )
        .wrap(Wrap { trim: true })
        .block(block);
        f.render_widget(p, area);
        return;
    }
    if app.claude_tool_rows.is_empty() {
        let p = Paragraph::new("(the server reported no tools)")
            .wrap(Wrap { trim: true })
            .block(block);
        f.render_widget(p, area);
        return;
    }
    let items: Vec<ListItem> = app
        .claude_tool_rows
        .iter()
        .enumerate()
        .map(|(i, name)| {
            let style = if i == app.claude_tool_sel {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };
            ListItem::new(Line::from(format!("  {name}"))).style(style)
        })
        .collect();
    f.render_widget(List::new(items).block(block), area);
}

fn render_debug(f: &mut Frame, app: &App, area: Rect) {
    let body = app.debug_text.clone().unwrap_or_else(|| {
        "Press 'r' to build the debug report (this can take a few seconds).".to_string()
    });
    // v0.22.0: discoverable copy hint — `[Y] Copy entire report` in the
    // top-right corner of the panel border title. ratatui Block titles are
    // shown in the top border; rendering on the right via title_alignment.
    let p = Paragraph::new(body)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Debug  (auto-sanitized; review before sharing publicly)")
                .title_top(Line::from("[Y] Copy entire report").right_aligned()),
        )
        .wrap(Wrap { trim: false })
        .scroll((app.debug_scroll, 0));
    f.render_widget(p, area);
}

/// v0.22.0 (Codex code-gate B4 R2): pure helper computing the inline row
/// decoration for the Update tab. Active in-flight overrides any stale
/// last-result for the same tool. Other tools' state never bleeds through.
///
/// - `tool`: the row's tool name (e.g. "rtk", "codex", "claude").
/// - `active`: `Some((active_tool, latest_stage))` when a worker is in flight
///   AND its `finished` is `None`. Caller filters this.
/// - `last_result`: `Some((tool, &Result))` when a previous run's outcome is
///   still being displayed.
///
/// Returns either an empty string or a leading-spaces decoration suffix.
fn rtk_row_decoration(
    tool: &str,
    active: Option<(&str, &str)>,
    last_result: Option<(&str, &Result<String, String>)>,
) -> String {
    if let Some((active_tool, stage)) = active {
        if active_tool == tool {
            return format!("  ... {stage}");
        }
    }
    if let Some((last_tool, outcome)) = last_result {
        if last_tool == tool {
            return match outcome {
                Ok(_) => "  [ok updated]".to_string(),
                Err(_) => "  [! update failed]".to_string(),
            };
        }
    }
    String::new()
}

/// v0.25.0: one-line summary of the staged self-update for the Update tab. Pure
/// (unit-tested). Returns `None` for a Superseded record (nothing useful to show).
/// v0.29 (B2): `running_version` is the plain semver of the BINARY currently running
/// this TUI (`aibridge_core::version()`). For a `Succeeded` update, once the running
/// binary is already at/above the staged `to`, the update is LIVE → we hide the
/// "restart to use it" nag (it persisted as a stale status even after the restart).
/// Fail-safe: if either version is unparseable we KEEP the nag (never hide a genuinely
/// pending restart on uncertainty).
fn staged_status_line(
    st: &aibridge_core::staged_update::UpdateStatus,
    running_version: &str,
) -> Option<String> {
    use aibridge_core::staged_update::StagedState;
    let vers = format!("{} → {}", st.from, st.to);
    Some(match st.state {
        StagedState::Staged => format!("staged update {vers} — starting updater…"),
        StagedState::Waiting => {
            format!("staged update {vers} — waiting for aibridge processes to exit ('x' to cancel)")
        }
        StagedState::Applying => format!("staged update {vers} — applying now…"),
        StagedState::Succeeded => {
            use aibridge_core::update::parse_version;
            let live = matches!(
                (parse_version(running_version), parse_version(&st.to)),
                (Some(running), Some(to)) if running >= to
            );
            if live {
                return None; // update already in effect — no restart needed
            }
            format!("updated to {} ✓ (restart to use it)", st.to)
        }
        StagedState::Failed => {
            let why = st.error.as_deref().unwrap_or("unknown error");
            format!("staged update {vers} FAILED: {why} ('g' to retry)")
        }
        StagedState::Superseded => return None,
    })
}

/// v0.26.0: pure status text for a CLI-update row. Single source of truth for the
/// label, driven by `CliCheck::status()` so a missing tool reads "not installed" and
/// an installed-but-unparsable tool reads "installed (version unknown)" — never a
/// misleading "up-to-date (?)" / "? → ?".
fn cli_row_status(c: &aibridge_core::cli_update::CliCheck) -> String {
    use aibridge_core::cli_update::CliStatus;
    let cur = c
        .current
        .as_ref()
        .map(|v| v.to_string())
        .unwrap_or_else(|| "?".into());
    let latest = c
        .latest
        .as_ref()
        .map(|v| v.to_string())
        .unwrap_or_else(|| "?".into());
    match c.status() {
        CliStatus::NotInstalled => match &c.latest {
            Some(l) => format!("not installed  (latest {l})"),
            None => "not installed".to_string(),
        },
        CliStatus::VersionUnknown => "installed  (version unknown)".to_string(),
        CliStatus::UpToDate => format!("up-to-date  ({cur})"),
        CliStatus::Outdated => format!("{cur} → {latest}"),
    }
}

/// v0.27.0: the optional "install Homebrew" affordance line for the Update tab.
/// `Some` only when [`can_offer_homebrew_install_ui`] holds (macOS + a missing
/// codex/claude with no package manager). Pure → unit-testable on any host.
fn homebrew_offer_line(
    is_macos: bool,
    checks: &[aibridge_core::cli_update::CliCheck],
) -> Option<String> {
    if aibridge_core::cli_update::can_offer_homebrew_install_ui(is_macos, checks) {
        Some(
            "  ↳ no package manager found — press 'b' to install Homebrew (opens Terminal)"
                .to_string(),
        )
    } else {
        None
    }
}

fn render_update(f: &mut Frame, app: &App, area: Rect) {
    let mut items: Vec<ListItem> = Vec::new();
    let sel = app.update_sel;

    // Row 0: aibridge self.
    let row0 = format!("  [aibridge]  installed {}", aibridge_core::VERSION_FULL);
    items.push(stylize_update_row(row0, sel == 0));
    if !app.update_line.is_empty() {
        items.push(ListItem::new(Line::from(Span::styled(
            format!("              {}", app.update_line),
            Style::default().fg(Color::DarkGray),
        ))));
    }
    // v0.25.0: staged-update status (in-TUI self-update; applied by the detached helper).
    if let Some(st) = &app.staged_status {
        if let Some(line) = staged_status_line(st, aibridge_core::version()) {
            items.push(ListItem::new(Line::from(Span::styled(
                format!("              {line}"),
                Style::default().fg(Color::Cyan),
            ))));
        }
    }

    // CLI checks.
    items.push(ListItem::new(Line::from("")));
    items.push(ListItem::new(Line::from(Span::styled(
        "CLI updates (codex / claude / rtk)",
        Style::default().fg(Color::DarkGray),
    ))));
    if app.cli_checks.is_empty() {
        items.push(ListItem::new(Line::from(Span::styled(
            "  (loading…)",
            Style::default().fg(Color::DarkGray),
        ))));
    }
    for (i, c) in app.cli_checks.iter().enumerate() {
        let row_idx = 1 + i;
        let status = cli_row_status(c);
        // v0.22.0 (Codex code-gate B4 R2): row decoration extracted to a pure
        // helper for unit testing of all 6 active/last branches.
        let active_args = app
            .active_cli_run
            .as_ref()
            .filter(|r| r.finished.is_none())
            .map(|r| (r.tool, r.latest_stage.as_str()));
        let last_args = app
            .last_cli_run_result
            .as_ref()
            .map(|r| (r.tool, &r.outcome));
        let decoration = rtk_row_decoration(c.tool, active_args, last_args);
        let line = format!(
            "  [{tool}]  {status}  via {src}{decoration}",
            tool = c.tool,
            src = c.source.label()
        );
        items.push(stylize_update_row(line, sel == row_idx));
        if let Some(note) = &c.manual_note {
            if !c.up_to_date() {
                items.push(ListItem::new(Line::from(Span::styled(
                    format!("              {note}"),
                    Style::default().fg(Color::DarkGray),
                ))));
            }
        }
    }

    // v0.27.0: macOS-only Homebrew install affordance (codex/claude missing, no PM).
    if let Some(offer) = homebrew_offer_line(cfg!(target_os = "macos"), &app.cli_checks) {
        items.push(ListItem::new(Line::from(Span::styled(
            offer,
            Style::default().fg(Color::Yellow),
        ))));
    }

    // MCP pins.
    if !app.mcp_pins.is_empty() {
        items.push(ListItem::new(Line::from("")));
        items.push(ListItem::new(Line::from(Span::styled(
            "MCP version pins (read-only)",
            Style::default().fg(Color::DarkGray),
        ))));
        for (i, p) in app.mcp_pins.iter().enumerate() {
            let row_idx = 1 + app.cli_checks.len() + i;
            let pin = p
                .version_pin
                .as_deref()
                .map(|v| format!("pinned={v}"))
                .unwrap_or_else(|| "auto-updates".into());
            let line = format!(
                "  [{agent}] {srv}  pkg={pkg}  {pin}",
                agent = p.agent,
                srv = p.server_name,
                pkg = p.package.as_deref().unwrap_or("?"),
            );
            items.push(stylize_update_row(line, sel == row_idx));
        }
    }

    items.push(ListItem::new(Line::from("")));
    let mut help =
        "↑/↓ navigate · 'c' check self · 'u' update selected row · 'r' re-check CLIs".to_string();
    if homebrew_offer_line(cfg!(target_os = "macos"), &app.cli_checks).is_some() {
        help.push_str(" · 'b' install Homebrew");
    }
    items.push(ListItem::new(Line::from(Span::styled(
        help,
        Style::default().fg(Color::DarkGray),
    ))));

    f.render_widget(
        List::new(items).block(
            Block::default()
                .borders(Borders::ALL)
                .title("Update  (aibridge update + CLI updates)"),
        ),
        area,
    );
}

fn stylize_update_row(line: String, selected: bool) -> ListItem<'static> {
    let style = if selected {
        Style::default().add_modifier(Modifier::REVERSED)
    } else {
        Style::default()
    };
    ListItem::new(Line::from(line)).style(style)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_app(names: &[&str]) -> App {
        App {
            cwd: "x".to_string(),
            tab: Tab::Health,
            checks: Vec::new(),
            health_scroll: 0,
            review: None,
            review_summary: None,
            // O1c: hermetic — empty picker state, no real-config IO in tests.
            model_rows: Vec::new(),
            model_current: review_mcp::CodexReviewConfig::default(),
            model_sel: 0,
            model_input: None,
            mcp_view: McpView::Servers,
            mcp_server: None,
            tool_rows: Vec::new(),
            tools_fresh: false,
            tools_known: false,
            tool_sel: 0,
            discover_rx: None,
            discovering: None,
            update_line: String::new(),
            update_rx: None,
            self_stage_rx: None,
            self_stage_starter: production_self_stage_starter(),
            staged_status: None,
            self_update_state: SelfUpdateState::Unprobed,
            update_check_kicked: false,
            clipboard: aibridge_platform::real_clipboard(),
            planner: std::sync::Arc::new(aibridge_core::update::plan_update),
            cli_checks: Vec::new(),
            cli_checks_rx: None,
            mcp_pins: Vec::new(),
            update_sel: 0,
            cli_update_starter: production_rtk_starter(),
            cli_stream_starter: production_cli_stream_starter(),
            fresh_install_armed: None,
            // v0.27.0: tests default to an INERT Homebrew opener that records nothing
            // and never spawns Terminal. Tests exercising the handoff install a fake.
            homebrew_opener: std::sync::Arc::new(|| Ok(())),
            brew_install_armed: false,
            active_cli_run: None,
            last_cli_run_result: None,
            pending_cli_recheck: false,
            // v0.22.0 (Codex code-gate R2 fix-up): test_app defaults to an INERT
            // CliChecksStarter that returns an immediately-disconnected receiver
            // (tx dropped) so `start_cli_checks` calls during tests never shell
            // out to real `gh`/`brew`/`npm`. Tests that need to drive the checks
            // lifecycle explicitly install `fake_checks_starter`.
            cli_checks_starter: std::sync::Arc::new(|| {
                let (_tx, rx) = std::sync::mpsc::channel();
                rx
            }),
            skills_report: None,
            skills_confirm: None,
            managed_apply_rx: None,
            managed_rows: Vec::new(),
            managed_sel: 0,
            managed_error: None,
            upstream_rx: None,
            upstream_candidates: std::collections::HashMap::new(),
            pending_bump: None,
            bump_rx: None,
            mcp: Some(
                names
                    .iter()
                    .map(|n| McpRow {
                        name: (*n).to_string(),
                        enabled: false,
                        interactive: false,
                    })
                    .collect(),
            ),
            mcp_sel: 0,
            claude_inventory: None,
            claude_inventory_error: None,
            claude_warnings: Vec::new(),
            claude_sel: 0,
            claude_view: McpView::Servers,
            claude_server: None,
            claude_tool_rows: Vec::new(),
            claude_tools_known: false,
            claude_tool_sel: 0,
            claude_discover_rx: None,
            claude_discovering: None,
            debug_text: None,
            debug_rx: None,
            debug_scroll: 0,
            message: None,
            quit: false,
        }
    }

    // ───────────────────── O1c: review-model picker (pure logic) ─────────────────────

    fn mi(slug: &str) -> codex_models::ModelInfo {
        codex_models::ModelInfo {
            slug: slug.to_string(),
            display_name: slug.to_string(),
            context_window: None,
            priority: 0,
        }
    }

    fn mk_cfg(model: Option<&str>, ctx: Option<u64>) -> review_mcp::CodexReviewConfig {
        review_mcp::CodexReviewConfig {
            model: model.map(str::to_string),
            model_context_window: ctx,
        }
    }

    #[test]
    fn model_choices_shape_empty_and_populated() {
        // Empty cache → [Default, Custom] (Custom is the fallback).
        let ch = model_choices(&[]);
        assert_eq!(ch.len(), 2);
        assert_eq!(ch[0], ModelChoice::Default);
        assert_eq!(ch[1], ModelChoice::Custom);
        // N rows → N + 2, models in the middle, Custom last.
        let rows = vec![mi("a"), mi("b"), mi("c")];
        let ch = model_choices(&rows);
        assert_eq!(ch.len(), 5);
        assert_eq!(ch[0], ModelChoice::Default);
        assert!(matches!(&ch[1], ModelChoice::Model(m) if m.slug == "a"));
        assert_eq!(ch[4], ModelChoice::Custom);
    }

    #[test]
    fn configured_choice_index_all_four_states() {
        let rows = vec![mi("a"), mi("b")];
        // unset → Default (0)
        assert_eq!(configured_choice_index(&rows, &mk_cfg(None, None)), 0);
        // a listed model → its row (1 + idx)
        assert_eq!(configured_choice_index(&rows, &mk_cfg(Some("b"), None)), 2);
        // an unlisted/custom model → the Custom row (last = rows.len() + 1)
        assert_eq!(
            configured_choice_index(&rows, &mk_cfg(Some("zzz"), None)),
            3
        );
        // empty cache + unset → Default
        assert_eq!(configured_choice_index(&[], &mk_cfg(None, None)), 0);
        // empty cache + custom → Custom (index 1)
        assert_eq!(configured_choice_index(&[], &mk_cfg(Some("zzz"), None)), 1);
        // a context override does NOT change which model is configured
        assert_eq!(
            configured_choice_index(&rows, &mk_cfg(Some("a"), Some(123))),
            1
        );
    }

    #[test]
    fn model_choice_set_args_preserves_ctx_except_default() {
        // Default clears BOTH model and context, even when a ctx is configured.
        assert_eq!(
            model_choice_set_args(&ModelChoice::Default, Some(123)),
            Some((None, None))
        );
        // A Model row PRESERVES the current context override (no silent clear).
        assert_eq!(
            model_choice_set_args(&ModelChoice::Model(mi("a")), Some(123)),
            Some((Some("a".to_string()), Some(123)))
        );
        assert_eq!(
            model_choice_set_args(&ModelChoice::Model(mi("a")), None),
            Some((Some("a".to_string()), None))
        );
        // Custom needs free-text input first (no direct persist).
        assert_eq!(model_choice_set_args(&ModelChoice::Custom, Some(123)), None);
    }

    #[test]
    fn edit_model_input_charset_backspace_enter_esc() {
        let mut b = String::new();
        for c in ['g', 'p', 't', '-', '5', '.'] {
            assert_eq!(
                edit_model_input(&mut b, KeyCode::Char(c)),
                InputOutcome::Editing
            );
        }
        assert_eq!(b, "gpt-5.");
        // disallowed chars (space, '!') are ignored — never enter the buffer.
        let _ = edit_model_input(&mut b, KeyCode::Char(' '));
        let _ = edit_model_input(&mut b, KeyCode::Char('!'));
        assert_eq!(b, "gpt-5.");
        // backspace pops.
        assert_eq!(
            edit_model_input(&mut b, KeyCode::Backspace),
            InputOutcome::Editing
        );
        assert_eq!(b, "gpt-5");
        // Enter submits the trimmed buffer.
        assert_eq!(
            edit_model_input(&mut b, KeyCode::Enter),
            InputOutcome::Submit("gpt-5".to_string())
        );
        // Esc cancels.
        assert_eq!(edit_model_input(&mut b, KeyCode::Esc), InputOutcome::Cancel);
    }

    #[test]
    fn realign_model_sel_centers_on_configured_row() {
        let mut a = test_app(&[]);
        a.model_rows = vec![mi("a"), mi("b")];
        a.model_current = mk_cfg(None, None);
        a.realign_model_sel();
        assert_eq!(a.model_sel, 0); // Default
        a.model_current = mk_cfg(Some("b"), None);
        a.realign_model_sel();
        assert_eq!(a.model_sel, 2); // listed row
        a.model_current = mk_cfg(Some("zzz"), None);
        a.realign_model_sel();
        assert_eq!(a.model_sel, 3); // Custom (unlisted)
        a.model_rows = vec![];
        a.model_current = mk_cfg(Some("zzz"), None);
        a.realign_model_sel();
        assert_eq!(a.model_sel, 1); // empty-cache Custom
    }

    #[test]
    fn enter_on_custom_row_opens_input_mode_no_io() {
        let mut a = test_app(&[]);
        a.tab = Tab::Review;
        a.model_rows = vec![mi("a")]; // choices = [Default, a, Custom]
        a.model_sel = 2; // Custom
        assert!(a.model_input.is_none());
        handle_key(&mut a, KeyCode::Enter);
        assert!(a.model_input.is_some(), "Enter on Custom opens the editor");
        assert!(!a.quit);
    }

    #[test]
    fn input_mode_owns_keys_no_quit_no_tab_switch() {
        let mut a = test_app(&[]);
        a.tab = Tab::Review;
        a.model_input = Some(String::new());
        // 'q' while typing must NOT quit — it is a valid slug char and goes to the buffer.
        handle_key(&mut a, KeyCode::Char('q'));
        assert!(!a.quit);
        assert!(a.tab == Tab::Review);
        assert_eq!(a.model_input.as_deref(), Some("q"));
        // Tab while typing must NOT switch tabs (ignored by the editor).
        handle_key(&mut a, KeyCode::Tab);
        assert!(a.tab == Tab::Review);
        assert_eq!(a.model_input.as_deref(), Some("q"));
        // Esc cancels input — does NOT quit.
        handle_key(&mut a, KeyCode::Esc);
        assert!(a.model_input.is_none());
        assert!(!a.quit);
    }

    #[test]
    fn submit_custom_model_keeps_editor_open_on_empty_or_error() {
        // `handle_key` `take()`s model_input before calling submit, so submit owns putting
        // it back. Empty + failed inputs must KEEP the editor open (not close + lose it).
        let mut a = test_app(&[]);
        a.tab = Tab::Review;
        // Empty submit → editor stays open with an empty buffer.
        a.model_input = None;
        a.submit_custom_model(String::new());
        assert_eq!(a.model_input.as_deref(), Some(""));
        // A slug that fails validation (space → rejected by set_codex_model BEFORE any
        // write — hermetic) restores the typed buffer so the user can fix it.
        a.model_input = None;
        a.submit_custom_model("bad slug!".to_string());
        assert_eq!(a.model_input.as_deref(), Some("bad slug!"));
        assert!(!a.quit);
    }

    #[test]
    fn tab_navigation_wraps_all_seven() {
        // Tab order: Health, Review, ClaudeMcpInspector, Mcp, Skills, Update, Debug.
        // (Claude before Codex per the user's macOS dogfood request.)
        let mut a = test_app(&[]);
        assert!(a.tab == Tab::Health);
        a.next_tab();
        assert!(a.tab == Tab::Review);
        a.next_tab();
        assert!(a.tab == Tab::ClaudeMcpInspector);
        a.next_tab();
        assert!(a.tab == Tab::Mcp);
        a.next_tab();
        assert!(a.tab == Tab::Skills);
        a.next_tab();
        assert!(a.tab == Tab::Update);
        a.next_tab();
        assert!(a.tab == Tab::Debug);
        a.next_tab();
        assert!(a.tab == Tab::Health); // wrap forward
        a.prev_tab();
        assert!(a.tab == Tab::Debug); // wrap backward
    }

    #[test]
    fn update_row_count_with_only_self() {
        let a = test_app(&[]);
        assert_eq!(a.update_row_count(), 1); // self only, no checks loaded yet
    }

    #[test]
    fn update_clamp_keeps_sel_in_range() {
        let mut a = test_app(&[]);
        a.update_sel = 99;
        a.clamp_update_sel();
        assert_eq!(a.update_sel, 0); // only the self row exists
    }

    #[test]
    fn update_u_on_self_row_with_newer_state_stages_in_tui() {
        // v0.25.0: pressing `u` on the self row with a `Newer` state STAGES the
        // update in-TUI (invokes the staging starter) and does NOT exit the TUI.
        let mut a = test_app(&[]);
        a.tab = Tab::Update;
        a.update_sel = 0;
        let invoked = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = invoked.clone();
        a.self_stage_starter = std::sync::Arc::new(move |_planned| {
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
            let (tx, rx) = std::sync::mpsc::channel();
            let _ = tx.send(Ok(()));
            rx
        });
        a.self_update_state = SelfUpdateState::Newer {
            from: "0.20.0".into(),
            to: "0.21.0".into(),
            summary: "test".into(),
            planned: aibridge_core::update::PlannedUpdate {
                install_path: std::path::PathBuf::from("/x/aibridge"),
                tag: "v0.21.0".into(),
                from: None,
                to: aibridge_core::update::parse_version("0.21.0").unwrap(),
            },
        };
        a.handle_update_action();
        assert!(
            invoked.load(std::sync::atomic::Ordering::SeqCst),
            "staging starter must be invoked"
        );
        assert!(!a.quit, "v0.25.0: self-update no longer exits the TUI");
        assert!(a.self_stage_rx.is_some(), "staging is in flight");
    }

    // ─── v0.25.0 staged self-update status line ───
    fn fake_status(
        state: aibridge_core::staged_update::StagedState,
    ) -> aibridge_core::staged_update::UpdateStatus {
        aibridge_core::staged_update::UpdateStatus {
            id: "id".into(),
            from: "0.24.0".into(),
            to: "0.25.0".into(),
            tag: "v0.25.0".into(),
            target: "/x/aibridge".into(),
            payload: "/x/.aibridge-staged-id".into(),
            state,
            error: Some("swap boom".into()),
            updated_ms: 0,
            heartbeat_ms: 0,
            helper_pid: None,
            helper_started_ms: None,
            spawn_attempted_ms: None,
            spawn_error: None,
        }
    }

    #[test]
    fn staged_status_line_waiting_mentions_cancel() {
        use aibridge_core::staged_update::StagedState;
        let line = super::staged_status_line(&fake_status(StagedState::Waiting), "0.0.0").unwrap();
        assert!(
            line.contains("waiting") && line.contains("'x' to cancel"),
            "{line}"
        );
    }

    #[test]
    fn staged_status_line_failed_mentions_retry_and_reason() {
        use aibridge_core::staged_update::StagedState;
        let line = super::staged_status_line(&fake_status(StagedState::Failed), "0.0.0").unwrap();
        assert!(
            line.contains("FAILED") && line.contains("swap boom") && line.contains("'g' to retry"),
            "{line}"
        );
    }

    #[test]
    fn staged_status_line_superseded_is_hidden() {
        use aibridge_core::staged_update::StagedState;
        assert!(
            super::staged_status_line(&fake_status(StagedState::Superseded), "0.0.0").is_none()
        );
    }

    // ─── v0.29 (B2): Succeeded nag hides once the running version IS the applied one ───
    #[test]
    fn staged_status_line_succeeded_shows_nag_when_not_yet_running_new_version() {
        use aibridge_core::staged_update::StagedState;
        // running 0.24.0 < to 0.25.0 → update not live yet → show "restart to use it"
        let line =
            super::staged_status_line(&fake_status(StagedState::Succeeded), "0.24.0").unwrap();
        assert!(line.contains("restart to use it"), "{line}");
    }

    #[test]
    fn staged_status_line_succeeded_hidden_when_running_equals_target() {
        use aibridge_core::staged_update::StagedState;
        // running == to (0.25.0) → live → no nag
        assert!(
            super::staged_status_line(&fake_status(StagedState::Succeeded), "0.25.0").is_none()
        );
    }

    #[test]
    fn staged_status_line_succeeded_hidden_when_running_newer_than_target() {
        use aibridge_core::staged_update::StagedState;
        // running 0.26.0 > to 0.25.0 → live → no nag
        assert!(
            super::staged_status_line(&fake_status(StagedState::Succeeded), "0.26.0").is_none()
        );
    }

    #[test]
    fn staged_status_line_succeeded_shows_nag_when_running_version_unparseable() {
        use aibridge_core::staged_update::StagedState;
        // can't confirm the update is live → fail-safe: keep the nag
        let line = super::staged_status_line(&fake_status(StagedState::Succeeded), "not-a-version")
            .unwrap();
        assert!(line.contains("restart to use it"), "{line}");
    }

    #[test]
    fn staged_status_line_succeeded_shows_nag_when_target_unparseable() {
        use aibridge_core::staged_update::StagedState;
        // `to` itself can't be parsed → can't confirm live → fail-safe: keep the nag
        let mut st = fake_status(StagedState::Succeeded);
        st.to = "weird-build".into();
        let line = super::staged_status_line(&st, "0.25.0").unwrap();
        assert!(line.contains("restart to use it"), "{line}");
    }

    // ─── v0.20.1 typed SelfUpdateState dispatch tests ───
    #[test]
    fn update_u_on_self_with_uptodate_does_not_quit() {
        let mut a = test_app(&[]);
        a.tab = Tab::Update;
        a.update_sel = 0;
        a.self_update_state = SelfUpdateState::UpToDate {
            reason: "Already on the latest release (0.20.1).".into(),
        };
        a.handle_update_action();
        assert!(!a.quit, "must NOT exit TUI when already on latest");
        let msg = a.message.unwrap_or_default();
        assert!(
            msg.contains("up to date"),
            "footer should mention 'up to date': {msg:?}"
        );
    }

    #[test]
    fn update_u_on_self_during_checking_does_not_quit() {
        let mut a = test_app(&[]);
        a.tab = Tab::Update;
        a.update_sel = 0;
        a.self_update_state = SelfUpdateState::Checking;
        a.handle_update_action();
        assert!(!a.quit);
        let msg = a.message.unwrap_or_default();
        assert!(msg.contains("checking"));
    }

    #[test]
    fn update_u_on_self_when_unprobed_kicks_probe_and_does_not_quit() {
        // Inject a fast fake planner so the spawned thread doesn't hit the network.
        let mut a = test_app(&[]);
        a.planner = std::sync::Arc::new(|_| {
            Ok(aibridge_core::update::UpdateDecision::Skip {
                reason: "test-fake".into(),
            })
        });
        a.tab = Tab::Update;
        a.update_sel = 0;
        a.self_update_state = SelfUpdateState::Unprobed;
        a.handle_update_action();
        assert!(!a.quit);
        assert!(a.update_rx.is_some(), "Unprobed `u` must START a probe");
        let msg = a.message.unwrap_or_default();
        assert!(msg.contains("probing") || msg.contains("try `u` again"));
    }

    #[test]
    fn update_u_on_self_with_error_does_not_quit() {
        let mut a = test_app(&[]);
        a.tab = Tab::Update;
        a.update_sel = 0;
        a.self_update_state = SelfUpdateState::Error {
            detail: "gh not on PATH".into(),
        };
        a.handle_update_action();
        assert!(!a.quit);
        let msg = a.message.unwrap_or_default();
        assert!(msg.contains("failed") && msg.contains("press `c`"));
    }

    // ─── v0.20.1 manual `c` key in-flight guard ───
    #[test]
    fn c_key_starts_fresh_probe_when_not_in_flight() {
        let mut a = test_app(&[]);
        a.planner = std::sync::Arc::new(|_| {
            Ok(aibridge_core::update::UpdateDecision::Skip {
                reason: "test".into(),
            })
        });
        a.tab = Tab::Update;
        assert!(a.update_rx.is_none());
        super::handle_key(&mut a, ratatui::crossterm::event::KeyCode::Char('c'));
        assert!(a.update_rx.is_some(), "c-key must start a probe");
        assert_eq!(a.self_update_state, SelfUpdateState::Checking);
    }

    #[test]
    fn c_key_refuses_when_probe_in_flight() {
        let mut a = test_app(&[]);
        a.tab = Tab::Update;
        // Plant a dummy in-flight receiver.
        let (_tx, rx) = std::sync::mpsc::channel();
        a.update_rx = Some(rx);
        let was_state = a.self_update_state.clone();
        super::handle_key(&mut a, ratatui::crossterm::event::KeyCode::Char('c'));
        // State unchanged, message set.
        assert_eq!(a.self_update_state, was_state);
        let msg = a.message.unwrap_or_default();
        assert!(msg.contains("already checking"));
    }

    // ─── v0.20.1 clipboard tests ───
    #[derive(Clone)]
    struct MockClipboard {
        captured: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        fail: bool,
    }
    impl aibridge_platform::ClipboardWriter for MockClipboard {
        fn copy(&self, text: &str) -> Result<usize, String> {
            if self.fail {
                return Err("mock failure".into());
            }
            self.captured.lock().unwrap().push(text.to_string());
            Ok(text.len())
        }
    }

    fn make_app_with_mock_clipboard() -> (App, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut a = test_app(&[]);
        a.clipboard = Box::new(MockClipboard {
            captured: captured.clone(),
            fail: false,
        });
        (a, captured)
    }

    #[test]
    fn debug_y_with_text_copies_via_mock_clipboard() {
        let (mut a, captured) = make_app_with_mock_clipboard();
        a.tab = Tab::Debug;
        a.debug_text = Some("hello-report".into());
        a.copy_debug_report_to_clipboard();
        let cap = captured.lock().unwrap();
        assert_eq!(cap.as_slice(), &["hello-report".to_string()]);
        let msg = a.message.unwrap_or_default();
        assert!(msg.contains("copied 12 bytes"));
    }

    #[test]
    fn debug_y_while_building_does_not_copy() {
        let (mut a, captured) = make_app_with_mock_clipboard();
        a.tab = Tab::Debug;
        a.debug_text = Some("placeholder".into());
        let (_tx, rx) = std::sync::mpsc::channel();
        a.debug_rx = Some(rx); // still building
        a.copy_debug_report_to_clipboard();
        assert!(
            captured.lock().unwrap().is_empty(),
            "must not copy while building"
        );
        let msg = a.message.unwrap_or_default();
        assert!(msg.contains("still building"));
    }

    #[test]
    fn debug_y_with_no_text_shows_hint() {
        let (mut a, captured) = make_app_with_mock_clipboard();
        a.tab = Tab::Debug;
        a.debug_text = None;
        a.copy_debug_report_to_clipboard();
        assert!(captured.lock().unwrap().is_empty());
        let msg = a.message.unwrap_or_default();
        assert!(msg.contains("press `r`"));
    }

    #[test]
    fn health_y_with_no_checks_shows_hint() {
        let (mut a, captured) = make_app_with_mock_clipboard();
        a.tab = Tab::Health;
        a.checks = Vec::new();
        a.copy_health_report_to_clipboard();
        assert!(captured.lock().unwrap().is_empty());
        let msg = a.message.unwrap_or_default();
        assert!(msg.contains("press `r`"));
    }

    #[test]
    fn health_y_with_checks_copies_plain_text() {
        let (mut a, captured) = make_app_with_mock_clipboard();
        a.tab = Tab::Health;
        a.checks = vec![aibridge_core::doctor::Check {
            name: "git".into(),
            status: aibridge_core::doctor::Status::Pass,
            detail: "available".into(),
        }];
        a.copy_health_report_to_clipboard();
        let cap = captured.lock().unwrap();
        assert_eq!(cap.len(), 1);
        let body = &cap[0];
        assert!(body.contains("AI Bridge"));
        assert!(body.contains("[ok  ] git"));
        assert!(body.contains("available"));
    }

    #[test]
    fn clipboard_failure_surfaces_in_footer() {
        let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut a = test_app(&[]);
        a.clipboard = Box::new(MockClipboard {
            captured: captured.clone(),
            fail: true,
        });
        a.tab = Tab::Debug;
        a.debug_text = Some("anything".into());
        a.copy_debug_report_to_clipboard();
        assert!(captured.lock().unwrap().is_empty());
        let msg = a.message.unwrap_or_default();
        assert!(msg.contains("clipboard write failed"));
        assert!(msg.contains("mock failure"));
    }

    #[test]
    fn update_u_on_cli_row_with_manual_source_does_not_mutate() {
        use aibridge_core::cli_update::{CliCheck, InstallSource};
        use aibridge_core::update::parse_version;
        let mut a = test_app(&[]);
        a.tab = Tab::Update;
        a.cli_checks = vec![CliCheck {
            tool: "claude",
            current: parse_version("2.1.146"),
            latest: parse_version("2.1.150"),
            source: InstallSource::NativeInstaller {
                docs_url: "https://claude.com/download".into(),
            },
            suggested_command: None,
            manual_note: Some("see https://claude.com/download".into()),
            installable: false,
            auto_confirm: false,
            installed: true,
        }];
        a.update_sel = 1; // first CLI row
        a.handle_update_action();
        // No mutation started, no exit triggered.
        assert!(!a.quit);
        assert!(a.active_cli_run.is_none());
        // But the user gets a footer message pointing at the docs URL.
        let m = a.message.unwrap_or_default();
        assert!(
            m.contains("manual update"),
            "footer should mention manual update: {m:?}"
        );
    }

    #[test]
    fn update_u_on_cli_row_with_verified_source_streams_in_tui() {
        // v0.25.0: a verified update streams in-TUI (no exit, no enqueue).
        use aibridge_core::cli_update::{CliCheck, InstallSource};
        use aibridge_core::update::parse_version;
        let (stream, recorded) = fake_stream_starter();
        let mut a = test_app(&[]);
        a.cli_stream_starter = stream;
        a.tab = Tab::Update;
        a.cli_checks = vec![CliCheck {
            tool: "codex",
            current: parse_version("0.130.0"),
            latest: parse_version("0.132.0"),
            source: InstallSource::Brew {
                package: "codex".into(),
            },
            suggested_command: Some(vec!["brew".into(), "upgrade".into(), "codex".into()]),
            manual_note: None,
            installable: false,
            auto_confirm: false,
            installed: true,
        }];
        a.update_sel = 1;
        a.handle_update_action();
        assert!(!a.quit, "v0.25.0: no shell drop");
        assert!(a.active_cli_run.is_some());
        assert_eq!(recorded.lock().unwrap().as_ref().unwrap().0, "codex");
    }

    #[test]
    fn update_u_on_mcp_pin_row_does_not_mutate() {
        use aibridge_core::cli_update::McpVersionStatus;
        let mut a = test_app(&[]);
        a.tab = Tab::Update;
        a.mcp_pins = vec![McpVersionStatus {
            agent: "claude",
            server_name: "shadcn".into(),
            package: Some("@some/pkg".into()),
            version_pin: Some("1.0.0".into()),
        }];
        a.update_sel = 1; // first MCP-pin row (cli_checks is empty here)
        a.handle_update_action();
        assert!(!a.quit);
        assert!(a.active_cli_run.is_none());
        let m = a.message.unwrap_or_default();
        assert!(
            m.contains("read-only"),
            "footer should mention read-only: {m:?}"
        );
    }

    #[test]
    fn claude_inspector_view_only_message_set_on_mutating_keys() {
        // Pressing Space anywhere on the Claude Inspector must produce the view-only
        // footer (not silence, not a no-op without feedback). Regression for the v1
        // scope contract.
        let mut a = test_app(&[]);
        a.tab = Tab::ClaudeMcpInspector;
        a.claude_view_only_message();
        let msg = a.message.unwrap_or_default();
        assert!(
            msg.contains("view-only"),
            "expected 'view-only' in footer, got: {msg:?}"
        );
        assert!(
            msg.contains("~/.claude.json"),
            "expected ~/.claude.json hint, got: {msg:?}"
        );
    }

    #[test]
    fn claude_inspector_view_prefers_warnings_over_empty_state() {
        // Codex Stop-gate F2 regression test: when servers list is empty BUT a
        // source had a read/parse error (warning present), the Inspector must
        // render the warning, NOT the cheerful "no MCPs configured" hint.
        let warnings = vec!["user-scope (~/.claude.json): parse error".to_string()];
        let v = claude_inspector_view(Some(&[]), None, &warnings);
        match v {
            InspectorView::WarningsOnly(w) => assert_eq!(w, warnings),
            other => panic!("expected WarningsOnly, got {other:?}"),
        }
    }

    #[test]
    fn claude_inspector_view_empty_no_warnings_is_empty_hint() {
        let v = claude_inspector_view(Some(&[]), None, &[]);
        assert_eq!(v, InspectorView::Empty);
    }

    #[test]
    fn claude_inspector_view_error_wins_over_everything() {
        let v = claude_inspector_view(Some(&[]), Some("oh no"), &["w".into()]);
        match v {
            InspectorView::Error(e) => assert_eq!(e, "oh no"),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn claude_inspector_view_servers_carries_warnings() {
        // Servers PLUS warnings → Servers variant carries the warnings forward, so
        // a corrupt user-scope alongside a working project-scope still gets a
        // visible yellow notice instead of being silently dropped.
        let rows = vec![claude_mcp::ClaudeServer {
            name: "x".into(),
            scope: claude_mcp::ClaudeScope::Project {
                source: std::path::PathBuf::from("/p/.mcp.json"),
            },
            transport: claude_mcp::Transport::Stdio {
                command: "echo".into(),
                args: vec![],
                env: vec![],
                cwd_field: None,
            },
            overrides_user: false,
        }];
        let warnings = vec!["user-scope (~/.claude.json): parse error".to_string()];
        let v = claude_inspector_view(Some(&rows), None, &warnings);
        match v {
            InspectorView::Servers { warnings: w } => assert_eq!(w, warnings),
            other => panic!("expected Servers, got {other:?}"),
        }
    }

    #[test]
    fn claude_back_unwinds_only_from_tools_view() {
        let mut a = test_app(&[]);
        a.tab = Tab::ClaudeMcpInspector;
        // From the server list, back does nothing (top-level Esc would quit).
        assert!(!a.claude_back());
        // Simulate having entered the tools view.
        a.claude_view = McpView::Tools;
        a.claude_server = Some(claude_mcp::ClaudeServer {
            name: "x".into(),
            scope: claude_mcp::ClaudeScope::User {
                source: std::path::PathBuf::from("/u/.claude.json"),
            },
            transport: claude_mcp::Transport::Stdio {
                command: "echo".into(),
                args: vec![],
                env: vec![],
                cwd_field: None,
            },
            overrides_user: false,
        });
        assert!(a.claude_back());
        assert!(matches!(a.claude_view, McpView::Servers));
        assert!(a.claude_server.is_none());
    }

    #[test]
    fn skills_confirm_is_two_key() {
        let mut a = test_app(&[]);
        // First press arms; second matching press confirms.
        assert!(!a.skills_confirm_press('s'));
        assert_eq!(a.skills_confirm, Some('s'));
        assert!(a.skills_confirm_press('s'));
        assert_eq!(a.skills_confirm, None); // cleared after confirm
                                            // A different action while one is armed re-arms (does NOT confirm).
        assert!(!a.skills_confirm_press('s'));
        assert!(!a.skills_confirm_press('m'));
        assert_eq!(a.skills_confirm, Some('m'));
    }

    #[test]
    fn in_flight_apply_blocks_mutating_skill_keys() {
        // Regression for the Codex finding: while a managed apply is in flight (the rx
        // channel is held), every mutating key on the Skills tab must NO-OP with a clear
        // "already in progress" footer message and NOT arm the 2-key confirm. This is the
        // same-process concurrency guard (the core ProcessLock is the cross-process one).
        let mut a = test_app(&[]);
        a.tab = Tab::Skills;
        a.managed_rows
            .push(aibridge_core::managed_skills::SkillStatus {
                name: "react-doctor".into(),
                enabled: true,
                state: "in sync".into(),
                attention: false,
                pin: "deadbeef".into(),
            });
        a.managed_sel = 0;
        let (_tx, rx) = std::sync::mpsc::channel::<String>();
        a.managed_apply_rx = Some(rx);
        for key in [
            KeyCode::Char('d'),
            KeyCode::Char('x'),
            KeyCode::Char('i'),
            KeyCode::Char('n'),
            KeyCode::Char('p'),
            KeyCode::Char('o'),
            KeyCode::Char('s'),
            KeyCode::Char('m'),
            KeyCode::Enter,
        ] {
            a.message = None;
            a.skills_confirm = None;
            handle_key(&mut a, key);
            assert!(
                a.message
                    .as_deref()
                    .unwrap_or("")
                    .contains("already in progress"),
                "key {key:?} did not surface the in-flight footer message"
            );
            assert_eq!(a.skills_confirm, None, "key {key:?} armed the confirm");
        }
    }

    #[test]
    fn mcp_selection_clamps() {
        let mut a = test_app(&["one", "two"]);
        a.tab = Tab::Mcp;
        assert_eq!(a.mcp_sel, 0);
        a.move_up(); // already at top → stays
        assert_eq!(a.mcp_sel, 0);
        a.move_down();
        assert_eq!(a.mcp_sel, 1);
        a.move_down(); // at bottom → stays (no index 2)
        assert_eq!(a.mcp_sel, 1);
        assert_eq!(a.selected_mcp().map(|(n, _)| n), Some("two"));
    }

    #[test]
    fn empty_mcp_has_no_selection() {
        let mut a = test_app(&[]);
        a.tab = Tab::Mcp;
        a.move_down();
        assert_eq!(a.mcp_sel, 0);
        assert!(a.selected_mcp().is_none());
    }

    #[test]
    fn mcp_tool_view_navigation_and_back() {
        let mut a = test_app(&["one", "two"]);
        a.tab = Tab::Mcp;
        // Enter opens the selected server's per-tool view; tool nav is separate.
        a.open_selected_server();
        assert!(a.mcp_view == McpView::Tools);
        assert_eq!(a.mcp_server.as_deref(), Some("one"));
        a.tool_rows = vec![("t1".into(), false), ("t2".into(), true)];
        a.move_down();
        assert_eq!(a.tool_sel, 1);
        a.move_down(); // clamp at the last tool
        assert_eq!(a.tool_sel, 1);
        a.move_up();
        assert_eq!(a.tool_sel, 0);
        // Esc backs out of the tool view (does not quit).
        assert!(a.mcp_back());
        assert!(a.mcp_view == McpView::Servers);
        assert!(a.mcp_server.is_none());
        // mcp_back from the server list does nothing (so top-level Esc would quit).
        assert!(!a.mcp_back());
    }

    #[test]
    fn movement_on_review_tab_is_noop() {
        let mut a = test_app(&["one"]);
        a.tab = Tab::Review;
        a.move_down();
        a.move_up();
        assert_eq!(a.mcp_sel, 0);
        assert_eq!(a.health_scroll, 0);
    }

    // ───────────────── v0.22.0: in-TUI rtk update tests ─────────────────

    type RecordedAction =
        std::sync::Arc<std::sync::Mutex<Option<aibridge_core::cli_update::RtkNativeAction>>>;
    type SavedTx =
        std::sync::Arc<std::sync::Mutex<Option<std::sync::mpsc::Sender<CliUpdateEvent>>>>;

    /// A fake CliUpdateStarter that records the action it was invoked with
    /// (in a shared Mutex) and returns a pre-filled receiver. Tests then drive
    /// the receiver by sending events through the saved Sender — NO real `gh`
    /// / runner / fs is ever touched.
    fn fake_starter() -> (CliUpdateStarter, RecordedAction, SavedTx) {
        let recorded_action: RecordedAction = std::sync::Arc::new(std::sync::Mutex::new(None));
        let saved_tx: SavedTx = std::sync::Arc::new(std::sync::Mutex::new(None));
        let action_for_closure = recorded_action.clone();
        let tx_for_closure = saved_tx.clone();
        let starter: CliUpdateStarter = std::sync::Arc::new(move |action| {
            *action_for_closure.lock().unwrap() = Some(action);
            let (tx, rx) = std::sync::mpsc::channel();
            *tx_for_closure.lock().unwrap() = Some(tx);
            // Dummy thread that exits immediately — no work is done in tests.
            let handle = std::thread::spawn(|| {});
            (rx, handle)
        });
        (starter, recorded_action, saved_tx)
    }

    /// v0.25.0: records the (tool, argv) a generic CLI-stream starter was invoked
    /// with, returning a pre-filled receiver. NEVER shells out.
    type RecordedStream = std::sync::Arc<std::sync::Mutex<Option<(&'static str, Vec<String>)>>>;
    fn fake_stream_starter() -> (CliStreamStarter, RecordedStream) {
        let recorded: RecordedStream = std::sync::Arc::new(std::sync::Mutex::new(None));
        let rec = recorded.clone();
        let starter: CliStreamStarter = std::sync::Arc::new(move |tool, argv| {
            *rec.lock().unwrap() = Some((tool, argv));
            let (_tx, rx) = std::sync::mpsc::channel();
            let handle = std::thread::spawn(|| {});
            (rx, handle)
        });
        (starter, recorded)
    }

    type ChecksStarterCallCount = std::sync::Arc<std::sync::atomic::AtomicUsize>;
    type SavedChecksTx = std::sync::Arc<
        std::sync::Mutex<Option<std::sync::mpsc::Sender<Vec<aibridge_core::cli_update::CliCheck>>>>,
    >;

    /// v0.22.0 (Codex code-gate B2 R2): fake CliChecksStarter that counts
    /// invocations and lets tests drive the rx via a captured Sender. NEVER
    /// shells out to real `gh`/`brew`/`npm`.
    fn fake_checks_starter() -> (CliChecksStarter, ChecksStarterCallCount, SavedChecksTx) {
        let count: ChecksStarterCallCount =
            std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let saved_tx: SavedChecksTx = std::sync::Arc::new(std::sync::Mutex::new(None));
        let count_for_closure = count.clone();
        let tx_for_closure = saved_tx.clone();
        let starter: CliChecksStarter = std::sync::Arc::new(move || {
            count_for_closure.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let (tx, rx) = std::sync::mpsc::channel();
            *tx_for_closure.lock().unwrap() = Some(tx);
            rx
        });
        (starter, count, saved_tx)
    }

    fn rtk_cli_check(
        source: aibridge_core::cli_update::InstallSource,
        installable: bool,
    ) -> aibridge_core::cli_update::CliCheck {
        use aibridge_core::cli_update::CliCheck;
        use aibridge_core::update::parse_version;
        // suggested_command must match safe_to_auto_run() rtk shape:
        //   [<current_exe>, "rtk", "update"|"install", "--yes"]
        let exe = aibridge_core::cli_update::current_exe_path().expect("test env has current_exe");
        let verb = if installable { "install" } else { "update" };
        CliCheck {
            tool: "rtk",
            current: if installable {
                None
            } else {
                parse_version("0.40.0")
            },
            latest: parse_version("0.42.0"),
            source,
            suggested_command: Some(vec![
                exe.display().to_string(),
                "rtk".into(),
                verb.into(),
                "--yes".into(),
            ]),
            manual_note: Some("press 'u' …".into()),
            installable,
            auto_confirm: false,
            // installable rtk = NotInstalled (missing); else native/brew install present.
            installed: !installable,
        }
    }

    #[test]
    fn update_u_on_rtk_native_invokes_starter_with_update_action() {
        use aibridge_core::cli_update::{InstallSource, RtkNativeAction};
        let (starter, recorded, _saved_tx) = fake_starter();
        let mut a = test_app(&[]);
        a.cli_update_starter = starter;
        a.tab = Tab::Update;
        a.cli_checks = vec![rtk_cli_check(
            InstallSource::NativeInstaller {
                docs_url: "auto-update target /usr/local/bin/rtk".into(),
            },
            false, // installable=false → Update action
        )];
        a.update_sel = 1;
        a.handle_update_action();
        assert!(
            a.active_cli_run.is_some(),
            "starter should populate active_cli_run"
        );
        assert!(!a.quit, "in-TUI rtk update must NOT exit");
        assert_eq!(
            *recorded.lock().unwrap(),
            Some(RtkNativeAction::Update),
            "starter must receive Update action"
        );
    }

    #[test]
    fn update_u_on_rtk_not_installed_invokes_starter_with_install_action() {
        use aibridge_core::cli_update::{InstallSource, RtkNativeAction};
        let (starter, recorded, _saved_tx) = fake_starter();
        let mut a = test_app(&[]);
        a.cli_update_starter = starter;
        a.tab = Tab::Update;
        a.cli_checks = vec![rtk_cli_check(
            InstallSource::Unknown {
                path: std::path::PathBuf::from("/usr/local/bin/rtk"),
                reason: "rtk not installed".into(),
            },
            true, // installable=true → Install action
        )];
        a.update_sel = 1;
        a.handle_update_action();
        assert!(a.active_cli_run.is_some());
        assert!(!a.quit);
        assert_eq!(*recorded.lock().unwrap(), Some(RtkNativeAction::Install));
    }

    #[test]
    fn update_u_on_rtk_brew_streams_in_tui() {
        // v0.25.0: brew-rtk no longer exits; it streams in-TUI via cli_stream_starter.
        use aibridge_core::cli_update::{CliCheck, InstallSource};
        use aibridge_core::update::parse_version;
        let (stream, recorded) = fake_stream_starter();
        let mut a = test_app(&[]);
        a.cli_stream_starter = stream;
        a.tab = Tab::Update;
        a.cli_checks = vec![CliCheck {
            tool: "rtk",
            current: parse_version("0.40.0"),
            latest: parse_version("0.42.0"),
            source: InstallSource::Brew {
                package: "rtk".into(),
            },
            suggested_command: Some(vec!["brew".into(), "upgrade".into(), "rtk".into()]),
            manual_note: None,
            installable: false,
            auto_confirm: false,
            installed: true,
        }];
        a.update_sel = 1;
        a.handle_update_action();
        assert!(!a.quit, "v0.25.0: brew rtk streams in-TUI, no exit");
        assert!(a.active_cli_run.is_some(), "in-TUI stream started");
        let rec = recorded.lock().unwrap();
        assert_eq!(rec.as_ref().unwrap().0, "rtk");
        assert_eq!(rec.as_ref().unwrap().1, vec!["brew", "upgrade", "rtk"]);
    }

    #[test]
    fn update_u_on_codex_brew_streams_in_tui() {
        use aibridge_core::cli_update::{CliCheck, InstallSource};
        use aibridge_core::update::parse_version;
        let (stream, recorded) = fake_stream_starter();
        let mut a = test_app(&[]);
        a.cli_stream_starter = stream;
        a.tab = Tab::Update;
        a.cli_checks = vec![CliCheck {
            tool: "codex",
            current: parse_version("0.130.0"),
            latest: parse_version("0.132.0"),
            source: InstallSource::Brew {
                package: "codex".into(),
            },
            suggested_command: Some(vec!["brew".into(), "upgrade".into(), "codex".into()]),
            manual_note: None,
            installable: false,
            auto_confirm: false,
            installed: true,
        }];
        a.update_sel = 1;
        a.handle_update_action();
        assert!(!a.quit, "codex/brew update streams in-TUI on single u");
        assert!(a.active_cli_run.is_some());
        assert_eq!(recorded.lock().unwrap().as_ref().unwrap().0, "codex");
    }

    #[test]
    fn update_u_on_codex_npm_streams_in_tui() {
        use aibridge_core::cli_update::{CliCheck, InstallSource};
        use aibridge_core::update::parse_version;
        let (stream, recorded) = fake_stream_starter();
        let mut a = test_app(&[]);
        a.cli_stream_starter = stream;
        a.tab = Tab::Update;
        a.cli_checks = vec![CliCheck {
            tool: "codex",
            current: parse_version("0.130.0"),
            latest: parse_version("0.132.0"),
            source: InstallSource::Npm {
                package: "@openai/codex".into(),
            },
            suggested_command: Some(vec![
                "npm".into(),
                "i".into(),
                "-g".into(),
                "@openai/codex@latest".into(),
            ]),
            manual_note: None,
            installable: false,
            auto_confirm: false,
            installed: true,
        }];
        a.update_sel = 1;
        a.handle_update_action();
        assert!(!a.quit);
        assert!(a.active_cli_run.is_some());
        assert!(recorded.lock().unwrap().is_some());
    }

    #[test]
    fn update_u_on_codex_fresh_install_brew_requires_two_keys() {
        // v0.25.0: FreshInstall keeps first-install friction via a 2-key confirm
        // (no shell drop). First `u` arms; second `u` starts the in-TUI stream.
        use aibridge_core::cli_update::{CliCheck, FreshInstallMethod, InstallSource};
        let (stream, recorded) = fake_stream_starter();
        let mut a = test_app(&[]);
        a.cli_stream_starter = stream;
        a.tab = Tab::Update;
        a.cli_checks = vec![CliCheck {
            tool: "codex",
            current: None,
            latest: None,
            source: InstallSource::FreshInstall {
                method: FreshInstallMethod::Brew {
                    package: "codex".into(),
                    is_cask: true,
                },
            },
            suggested_command: Some(vec![
                "brew".into(),
                "install".into(),
                "--cask".into(),
                "codex".into(),
            ]),
            manual_note: None,
            installable: true,
            auto_confirm: false,
            installed: false,
        }];
        a.update_sel = 1;
        // First press → arms, does NOT run.
        a.handle_update_action();
        assert!(
            a.active_cli_run.is_none(),
            "first u must NOT run a fresh install"
        );
        assert_eq!(a.fresh_install_armed, Some("codex"));
        assert!(recorded.lock().unwrap().is_none());
        // Second press → starts the in-TUI stream.
        a.handle_update_action();
        assert!(a.active_cli_run.is_some(), "second u starts the install");
        assert_eq!(a.fresh_install_armed, None);
        assert_eq!(recorded.lock().unwrap().as_ref().unwrap().0, "codex");
        assert!(!a.quit);
    }

    #[test]
    fn update_u_on_codex_fresh_install_npm_requires_two_keys() {
        use aibridge_core::cli_update::{CliCheck, FreshInstallMethod, InstallSource};
        let (stream, recorded) = fake_stream_starter();
        let mut a = test_app(&[]);
        a.cli_stream_starter = stream;
        a.tab = Tab::Update;
        a.cli_checks = vec![CliCheck {
            tool: "codex",
            current: None,
            latest: None,
            source: InstallSource::FreshInstall {
                method: FreshInstallMethod::Npm {
                    package: "@openai/codex".into(),
                },
            },
            suggested_command: Some(vec![
                "npm".into(),
                "install".into(),
                "-g".into(),
                "@openai/codex".into(),
            ]),
            manual_note: None,
            installable: true,
            auto_confirm: false,
            installed: false,
        }];
        a.update_sel = 1;
        a.handle_update_action();
        assert!(a.active_cli_run.is_none());
        assert_eq!(a.fresh_install_armed, Some("codex"));
        a.handle_update_action();
        assert!(a.active_cli_run.is_some());
        assert!(recorded.lock().unwrap().is_some());
    }

    #[test]
    fn fresh_install_arm_cancelled_by_any_other_key() {
        // v0.25.0 code-gate fix: an intervening non-`u` key (c/r/x/g) cancels the
        // FreshInstall arm so the next `u` is a fresh FIRST press (re-arms, no run).
        use aibridge_core::cli_update::{CliCheck, FreshInstallMethod, InstallSource};
        for cancel_key in ['c', 'r', 'x', 'g'] {
            let (stream, recorded) = fake_stream_starter();
            let mut a = test_app(&[]);
            a.cli_stream_starter = stream;
            // Avoid network: make `c` (self-update probe) a no-op planner.
            a.planner = std::sync::Arc::new(|_| {
                Ok(aibridge_core::update::UpdateDecision::Skip {
                    reason: "test".into(),
                })
            });
            a.tab = Tab::Update;
            a.cli_checks = vec![CliCheck {
                tool: "codex",
                current: None,
                latest: None,
                source: InstallSource::FreshInstall {
                    method: FreshInstallMethod::Npm {
                        package: "@openai/codex".into(),
                    },
                },
                suggested_command: Some(vec![
                    "npm".into(),
                    "install".into(),
                    "-g".into(),
                    "@openai/codex".into(),
                ]),
                manual_note: None,
                installable: true,
                auto_confirm: false,
                installed: false,
            }];
            a.update_sel = 1;
            // First `u` arms.
            super::handle_key(&mut a, ratatui::crossterm::event::KeyCode::Char('u'));
            assert_eq!(
                a.fresh_install_armed,
                Some("codex"),
                "key {cancel_key}: armed"
            );
            // Intervening key cancels the arm.
            super::handle_key(&mut a, ratatui::crossterm::event::KeyCode::Char(cancel_key));
            assert_eq!(
                a.fresh_install_armed, None,
                "key {cancel_key}: arm must be cancelled"
            );
            // The next `u` must NOT immediately run a first install (the arm is gone,
            // so it's treated as a fresh first press → re-arm, never an instant run).
            super::handle_key(&mut a, ratatui::crossterm::event::KeyCode::Char('u'));
            assert!(
                a.active_cli_run.is_none(),
                "key {cancel_key}: must not start an install after a cancelled arm"
            );
            assert!(
                recorded.lock().unwrap().is_none(),
                "key {cancel_key}: no stream"
            );
        }
    }

    #[test]
    fn update_u_when_safe_to_auto_run_false_does_not_mutate() {
        use aibridge_core::cli_update::{CliCheck, InstallSource};
        use aibridge_core::update::parse_version;
        let (starter, recorded, _) = fake_starter();
        let mut a = test_app(&[]);
        a.cli_update_starter = starter;
        a.tab = Tab::Update;
        // Brew source but suggested_command shape doesn't pass safe_to_auto_run
        // (path 1 needs is_verified_pkg_manager which Brew IS, so this passes…).
        // To force false: use an Unknown source with NO suggested_command.
        a.cli_checks = vec![CliCheck {
            tool: "rtk",
            current: parse_version("0.40.0"),
            latest: parse_version("0.42.0"),
            source: InstallSource::Unknown {
                path: std::path::PathBuf::from("/usr/local/bin/rtk"),
                reason: "test".into(),
            },
            suggested_command: None,
            manual_note: Some("manual update — test".into()),
            installable: false,
            auto_confirm: false,
            installed: true,
        }];
        a.update_sel = 1;
        a.handle_update_action();
        assert!(!a.quit);
        assert!(a.active_cli_run.is_none());
        assert!(recorded.lock().unwrap().is_none());
    }

    #[test]
    fn update_u_on_version_unknown_does_not_mutate() {
        // v0.26.0: installed-but-version-unknown (current=None, latest=Some, Brew,
        // safe suggested_command) must NOT auto-run and must NOT say "up to date".
        use aibridge_core::cli_update::{CliCheck, InstallSource};
        use aibridge_core::update::parse_version;
        let (stream, recorded) = fake_stream_starter();
        let mut a = test_app(&[]);
        a.cli_stream_starter = stream;
        a.tab = Tab::Update;
        a.cli_checks = vec![CliCheck {
            tool: "codex",
            current: None,
            latest: parse_version("0.132.0"),
            source: InstallSource::Brew {
                package: "codex".into(),
            },
            suggested_command: Some(vec!["brew".into(), "upgrade".into(), "codex".into()]),
            manual_note: None,
            installable: false,
            auto_confirm: false,
            installed: true,
        }];
        a.update_sel = 1;
        a.handle_update_action();
        assert!(
            a.active_cli_run.is_none(),
            "version-unknown must not start a run"
        );
        assert!(
            recorded.lock().unwrap().is_none(),
            "stream starter not called"
        );
        assert!(!a.quit);
        let m = a.message.unwrap_or_default();
        assert!(m.contains("version unknown"), "footer: {m}");
        assert!(!m.contains("up to date"), "must not say up to date: {m}");
    }

    #[test]
    fn update_u_on_up_to_date_says_nothing_to_do() {
        use aibridge_core::cli_update::{CliCheck, InstallSource};
        use aibridge_core::update::parse_version;
        let (stream, recorded) = fake_stream_starter();
        let mut a = test_app(&[]);
        a.cli_stream_starter = stream;
        a.tab = Tab::Update;
        a.cli_checks = vec![CliCheck {
            tool: "codex",
            current: parse_version("0.132.0"),
            latest: parse_version("0.132.0"),
            source: InstallSource::Brew {
                package: "codex".into(),
            },
            suggested_command: Some(vec!["brew".into(), "upgrade".into(), "codex".into()]),
            manual_note: None,
            installable: false,
            auto_confirm: false,
            installed: true,
        }];
        a.update_sel = 1;
        a.handle_update_action();
        assert!(a.active_cli_run.is_none());
        assert!(recorded.lock().unwrap().is_none());
        let m = a.message.unwrap_or_default();
        assert!(m.contains("up to date"), "footer: {m}");
    }

    // ─── v0.26.0 cli_row_status (pure formatter) ───
    fn status_check(
        installed: bool,
        cur: Option<&str>,
        latest: Option<&str>,
    ) -> aibridge_core::cli_update::CliCheck {
        use aibridge_core::cli_update::{CliCheck, InstallSource};
        use aibridge_core::update::parse_version;
        CliCheck {
            tool: "x",
            current: cur.and_then(parse_version),
            latest: latest.and_then(parse_version),
            source: InstallSource::Brew {
                package: "x".into(),
            },
            suggested_command: None,
            manual_note: None,
            installable: false,
            auto_confirm: false,
            installed,
        }
    }

    #[test]
    fn cli_row_status_renders_all_states() {
        assert_eq!(
            super::cli_row_status(&status_check(false, None, None)),
            "not installed"
        );
        assert_eq!(
            super::cli_row_status(&status_check(false, None, Some("0.42.0"))),
            "not installed  (latest 0.42.0)"
        );
        assert_eq!(
            super::cli_row_status(&status_check(true, None, None)),
            "installed  (version unknown)"
        );
        assert_eq!(
            super::cli_row_status(&status_check(true, Some("1.0.0"), Some("1.0.0"))),
            "up-to-date  (1.0.0)"
        );
        assert_eq!(
            super::cli_row_status(&status_check(true, Some("1.0.0"), Some("1.1.0"))),
            "1.0.0 → 1.1.0"
        );
    }

    #[test]
    fn update_u_while_active_run_in_flight_does_not_spawn_second() {
        use aibridge_core::cli_update::{InstallSource, RtkNativeAction};
        let (starter, recorded, _saved_tx) = fake_starter();
        let mut a = test_app(&[]);
        a.cli_update_starter = starter;
        a.tab = Tab::Update;
        a.cli_checks = vec![rtk_cli_check(
            InstallSource::NativeInstaller {
                docs_url: "auto-update target /usr/local/bin/rtk".into(),
            },
            false,
        )];
        a.update_sel = 1;
        a.handle_update_action();
        assert_eq!(*recorded.lock().unwrap(), Some(RtkNativeAction::Update));
        // Reset recorded and press again — should NOT spawn a second run.
        *recorded.lock().unwrap() = None;
        a.handle_update_action();
        assert!(
            recorded.lock().unwrap().is_none(),
            "second press must not invoke starter while active"
        );
        let msg = a.message.unwrap_or_default();
        assert!(msg.contains("update in progress"), "footer: {msg}");
    }

    #[test]
    fn poll_active_cli_run_drains_to_done_and_populates_last_result() {
        use aibridge_core::cli_update::{InstallSource, RtkNativeAction};
        let (starter, _recorded, saved_tx) = fake_starter();
        let mut a = test_app(&[]);
        a.cli_update_starter = starter;
        a.tab = Tab::Update;
        a.cli_checks = vec![rtk_cli_check(
            InstallSource::NativeInstaller {
                docs_url: "auto-update target /usr/local/bin/rtk".into(),
            },
            false,
        )];
        a.update_sel = 1;
        a.handle_update_action();
        // Drive the worker via the captured Sender.
        let tx_guard = saved_tx.lock().unwrap();
        let tx = tx_guard.as_ref().unwrap().clone();
        drop(tx_guard);
        tx.send(CliUpdateEvent::Started).unwrap();
        tx.send(CliUpdateEvent::Stage("downloading".into()))
            .unwrap();
        tx.send(CliUpdateEvent::Done(Ok("rtk installed at /…".into())))
            .unwrap();
        a.poll_active_cli_run();
        assert!(
            a.active_cli_run.is_none(),
            "active_cli_run must be cleared on Done"
        );
        assert!(
            a.last_cli_run_result
                .as_ref()
                .is_some_and(|r| r.outcome.is_ok()),
            "last_cli_run_result must hold Ok outcome"
        );
        let msg = a.message.unwrap_or_default();
        assert!(msg.contains("rtk updated"), "footer: {msg}");
        let _ = RtkNativeAction::Update; // silence unused-import on cfg paths
    }

    #[test]
    fn poll_active_cli_run_drains_to_done_err_clears_active_and_records() {
        use aibridge_core::cli_update::InstallSource;
        let (starter, _recorded, saved_tx) = fake_starter();
        let mut a = test_app(&[]);
        a.cli_update_starter = starter;
        a.tab = Tab::Update;
        a.cli_checks = vec![rtk_cli_check(
            InstallSource::NativeInstaller {
                docs_url: "auto-update target /usr/local/bin/rtk".into(),
            },
            false,
        )];
        a.update_sel = 1;
        a.handle_update_action();
        let tx_guard = saved_tx.lock().unwrap();
        let tx = tx_guard.as_ref().unwrap().clone();
        drop(tx_guard);
        tx.send(CliUpdateEvent::Done(Err("download failed".into())))
            .unwrap();
        a.poll_active_cli_run();
        assert!(a.active_cli_run.is_none());
        assert!(a
            .last_cli_run_result
            .as_ref()
            .is_some_and(|r| r.outcome.is_err()));
        let msg = a.message.unwrap_or_default();
        assert!(msg.contains("rtk update failed"), "footer: {msg}");
    }

    #[test]
    fn poll_active_cli_run_updates_latest_stage_on_progress_events() {
        use aibridge_core::cli_update::InstallSource;
        let (starter, _recorded, saved_tx) = fake_starter();
        let mut a = test_app(&[]);
        a.cli_update_starter = starter;
        a.tab = Tab::Update;
        a.cli_checks = vec![rtk_cli_check(
            InstallSource::NativeInstaller {
                docs_url: "auto-update target /usr/local/bin/rtk".into(),
            },
            false,
        )];
        a.update_sel = 1;
        a.handle_update_action();
        let tx_guard = saved_tx.lock().unwrap();
        let tx = tx_guard.as_ref().unwrap().clone();
        drop(tx_guard);
        tx.send(CliUpdateEvent::Started).unwrap();
        tx.send(CliUpdateEvent::Stage("verifying SHA256".into()))
            .unwrap();
        a.poll_active_cli_run();
        assert!(a.active_cli_run.is_some(), "still in flight (no Done yet)");
        assert_eq!(
            a.active_cli_run.as_ref().unwrap().latest_stage,
            "verifying SHA256"
        );
    }

    #[test]
    fn footer_help_for_debug_mentions_y_copy_all() {
        // Verifies the Debug footer help string surfaces the `y` key. This
        // string lives in the per-tab match below `render` — extract via the
        // same hard-coded constant to avoid drift.
        let s = "Tab/Left/Right: tabs | Up/Down: scroll | y: copy all to clipboard | r: rebuild | q: quit";
        assert!(s.contains("y: copy all"));
    }

    #[test]
    fn footer_help_for_health_mentions_y_copy_all() {
        let s = "Tab/Left/Right: tabs | Up/Down: scroll | y: copy all to clipboard | r: refresh | q: quit";
        assert!(s.contains("y: copy all"));
    }

    // ────────── Code-gate B4: actual key dispatch + render helper tests ──────────

    #[test]
    fn quit_blocked_while_active_run_unfinished() {
        use aibridge_core::cli_update::InstallSource;
        let (starter, _r, _s) = fake_starter();
        let mut a = test_app(&[]);
        a.cli_update_starter = starter;
        a.tab = Tab::Update;
        a.cli_checks = vec![rtk_cli_check(
            InstallSource::NativeInstaller {
                docs_url: "auto-update target /usr/local/bin/rtk".into(),
            },
            false,
        )];
        a.update_sel = 1;
        a.handle_update_action();
        assert!(a.active_cli_run.is_some());
        // Drive the actual key handler — `q` should NOT quit.
        handle_key(&mut a, KeyCode::Char('q'));
        assert!(!a.quit, "q must be blocked while active");
        let msg = a.message.unwrap_or_default();
        assert!(msg.contains("update in progress"), "footer: {msg}");
    }

    #[test]
    fn esc_blocked_while_active_at_top_level() {
        use aibridge_core::cli_update::InstallSource;
        let (starter, _r, _s) = fake_starter();
        let mut a = test_app(&[]);
        a.cli_update_starter = starter;
        a.tab = Tab::Update; // not in per-tool view → Esc goes to top-level quit branch
        a.cli_checks = vec![rtk_cli_check(
            InstallSource::NativeInstaller {
                docs_url: "auto-update target /usr/local/bin/rtk".into(),
            },
            false,
        )];
        a.update_sel = 1;
        a.handle_update_action();
        handle_key(&mut a, KeyCode::Esc);
        assert!(!a.quit, "Esc must be blocked while active");
    }

    #[test]
    fn quit_works_after_active_finishes() {
        use aibridge_core::cli_update::InstallSource;
        let (starter, _r, saved_tx) = fake_starter();
        let mut a = test_app(&[]);
        a.cli_update_starter = starter;
        a.tab = Tab::Update;
        a.cli_checks = vec![rtk_cli_check(
            InstallSource::NativeInstaller {
                docs_url: "auto-update target /usr/local/bin/rtk".into(),
            },
            false,
        )];
        a.update_sel = 1;
        a.handle_update_action();
        let tx = saved_tx.lock().unwrap().as_ref().unwrap().clone();
        tx.send(CliUpdateEvent::Done(Ok("done".into()))).unwrap();
        a.poll_active_cli_run();
        assert!(a.active_cli_run.is_none(), "active cleared on Done");
        handle_key(&mut a, KeyCode::Char('q'));
        assert!(a.quit, "q must work after Done observed");
    }

    #[test]
    fn last_cli_run_result_cleared_on_next_u_same_tool() {
        use aibridge_core::cli_update::InstallSource;
        let (starter, _r, saved_tx) = fake_starter();
        let mut a = test_app(&[]);
        a.cli_update_starter = starter;
        a.tab = Tab::Update;
        a.cli_checks = vec![rtk_cli_check(
            InstallSource::NativeInstaller {
                docs_url: "auto-update target /usr/local/bin/rtk".into(),
            },
            false,
        )];
        a.update_sel = 1;
        a.handle_update_action();
        let tx = saved_tx.lock().unwrap().as_ref().unwrap().clone();
        tx.send(CliUpdateEvent::Done(Ok("done".into()))).unwrap();
        a.poll_active_cli_run();
        assert!(
            a.last_cli_run_result.is_some(),
            "first run populates last_result"
        );
        // Press u again on same rtk row. Stale last_result for "rtk" must be
        // cleared by the handler before spawning the second run.
        a.handle_update_action();
        assert!(
            a.last_cli_run_result.is_none(),
            "second u on same tool must clear stale last_result"
        );
        assert!(a.active_cli_run.is_some(), "second run started");
    }

    #[test]
    fn disconnect_without_done_synthesizes_failure() {
        use aibridge_core::cli_update::InstallSource;
        let (starter, _r, saved_tx) = fake_starter();
        let mut a = test_app(&[]);
        a.cli_update_starter = starter;
        a.tab = Tab::Update;
        a.cli_checks = vec![rtk_cli_check(
            InstallSource::NativeInstaller {
                docs_url: "auto-update target /usr/local/bin/rtk".into(),
            },
            false,
        )];
        a.update_sel = 1;
        a.handle_update_action();
        // Drop ALL senders without sending Done → channel disconnects.
        *saved_tx.lock().unwrap() = None;
        a.poll_active_cli_run();
        assert!(
            a.active_cli_run.is_none(),
            "disconnect must clear active_cli_run"
        );
        assert!(
            a.last_cli_run_result
                .as_ref()
                .is_some_and(|r| r.outcome.is_err()),
            "disconnect must synthesize Done(Err)"
        );
        let msg = a.message.clone().unwrap_or_default();
        assert!(
            msg.contains("worker exited without completing"),
            "footer must explain disconnect: {msg}"
        );
        // And q is now unblocked.
        handle_key(&mut a, KeyCode::Char('q'));
        assert!(a.quit, "q must work after synthesized failure");
    }

    #[test]
    fn pending_recheck_kicks_after_in_flight_check_completes() {
        use aibridge_core::cli_update::InstallSource;
        let (starter, _r, saved_tx) = fake_starter();
        let (checks_starter, count, saved_checks_tx) = fake_checks_starter();
        let mut a = test_app(&[]);
        a.cli_update_starter = starter;
        a.cli_checks_starter = checks_starter;
        a.tab = Tab::Update;
        a.cli_checks = vec![rtk_cli_check(
            InstallSource::NativeInstaller {
                docs_url: "auto-update target /usr/local/bin/rtk".into(),
            },
            false,
        )];
        a.update_sel = 1;
        // Pre-populate cli_checks_rx as if a check were already in flight.
        a.start_cli_checks();
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(a.cli_checks_rx.is_some());
        // Now start the rtk update and drive it to Done.
        a.handle_update_action();
        let tx = saved_tx.lock().unwrap().as_ref().unwrap().clone();
        tx.send(CliUpdateEvent::Done(Ok("done".into()))).unwrap();
        a.poll_active_cli_run();
        assert!(
            a.pending_cli_recheck,
            "pending_cli_recheck must be set when checks_rx is busy"
        );
        assert_eq!(
            count.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "no extra check yet — still draining the in-flight one"
        );
        // Now complete the in-flight check. The next poll_update drain should
        // clear cli_checks_rx, see pending_cli_recheck, and kick a fresh start.
        let checks_tx = saved_checks_tx.lock().unwrap().as_ref().unwrap().clone();
        checks_tx.send(Vec::new()).unwrap();
        a.poll_update();
        assert!(
            !a.pending_cli_recheck,
            "pending flag must be cleared by drain hook"
        );
        assert_eq!(
            count.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "starter must be invoked a second time for the post-update re-check"
        );
        assert!(
            a.cli_checks_rx.is_some(),
            "new rx installed by post-update check"
        );
    }

    // ───── pure-helper render decoration tests (C-render-pure-helper) ─────

    #[test]
    fn rtk_row_decoration_empty_when_no_active_or_last() {
        assert_eq!(rtk_row_decoration("rtk", None, None), "");
    }

    #[test]
    fn rtk_row_decoration_ok_when_last_outcome_ok() {
        let outcome = Ok("done".to_string());
        let s = rtk_row_decoration("rtk", None, Some(("rtk", &outcome)));
        assert!(s.contains("[ok updated]"), "got: {s}");
    }

    #[test]
    fn rtk_row_decoration_err_when_last_outcome_err() {
        let outcome = Err("nope".to_string());
        let s = rtk_row_decoration("rtk", None, Some(("rtk", &outcome)));
        assert!(s.contains("[! update failed]"), "got: {s}");
    }

    #[test]
    fn rtk_row_decoration_active_shows_stage_for_matching_tool() {
        let s = rtk_row_decoration("rtk", Some(("rtk", "downloading")), None);
        assert!(s.contains("downloading"), "got: {s}");
        assert!(s.contains("..."), "got: {s}");
    }

    #[test]
    fn rtk_row_decoration_other_tools_get_no_decoration() {
        let outcome = Ok("done".to_string());
        let s = rtk_row_decoration(
            "codex",
            Some(("rtk", "downloading")),
            Some(("rtk", &outcome)),
        );
        assert_eq!(s, "", "codex row must not show rtk's state");
    }

    #[test]
    fn rtk_row_decoration_active_wins_over_stale_last_for_same_tool() {
        let outcome = Ok("done".to_string());
        let s = rtk_row_decoration(
            "rtk",
            Some(("rtk", "verifying SHA256")),
            Some(("rtk", &outcome)),
        );
        assert!(s.contains("verifying SHA256"), "active wins: {s}");
        assert!(!s.contains("[ok updated]"), "stale last hidden: {s}");
    }

    // ───── v0.27.0 Homebrew Terminal-handoff (`b` on the Update tab) ─────

    type HomebrewCallCount = std::sync::Arc<std::sync::atomic::AtomicUsize>;

    /// Fake opener: records call count, returns `result` each time, never spawns
    /// Terminal.
    fn fake_homebrew_opener(result: Result<(), String>) -> (HomebrewOpener, HomebrewCallCount) {
        let count: HomebrewCallCount = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let c = count.clone();
        let opener: HomebrewOpener = std::sync::Arc::new(move || {
            c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            result.clone()
        });
        (opener, count)
    }

    fn codex_missing_no_pm() -> aibridge_core::cli_update::CliCheck {
        use aibridge_core::cli_update::{CliCheck, InstallSource};
        CliCheck {
            tool: "codex",
            current: None,
            latest: None,
            source: InstallSource::Unknown {
                path: std::path::PathBuf::from("codex"),
                reason: "neither brew nor npm".into(),
            },
            suggested_command: None,
            manual_note: None,
            installable: false,
            auto_confirm: false,
            installed: false,
        }
    }

    #[test]
    fn homebrew_offer_line_only_on_macos_with_missing_codex_no_pm() {
        let checks = vec![codex_missing_no_pm()];
        assert!(
            super::homebrew_offer_line(true, &checks).is_some(),
            "macOS offers"
        );
        assert!(
            super::homebrew_offer_line(false, &checks).is_none(),
            "off-macOS never offers"
        );
        // A normal installed tool (package manager present) → no offer even on macOS.
        let installed = vec![status_check(true, Some("1.0.0"), Some("1.0.0"))];
        assert!(super::homebrew_offer_line(true, &installed).is_none());
    }

    #[test]
    fn homebrew_b_two_key_confirm_opens_terminal() {
        use std::sync::atomic::Ordering::SeqCst;
        let (opener, count) = fake_homebrew_opener(Ok(()));
        let mut a = test_app(&[]);
        a.homebrew_opener = opener;
        a.tab = Tab::Update;
        a.cli_checks = vec![codex_missing_no_pm()];
        // First press arms; the opener is NOT called.
        a.handle_homebrew_install(true);
        assert!(a.brew_install_armed, "first b arms");
        assert_eq!(count.load(SeqCst), 0, "opener not called on first press");
        // Second press hands off to Terminal.
        a.handle_homebrew_install(true);
        assert!(!a.brew_install_armed, "disarmed after open");
        assert_eq!(count.load(SeqCst), 1, "opener called exactly once");
        let m = a.message.unwrap_or_default();
        assert!(m.contains("press `r`"), "footer points at re-check: {m}");
    }

    #[test]
    fn homebrew_b_off_macos_does_not_open() {
        use std::sync::atomic::Ordering::SeqCst;
        let (opener, count) = fake_homebrew_opener(Ok(()));
        let mut a = test_app(&[]);
        a.homebrew_opener = opener;
        a.tab = Tab::Update;
        a.cli_checks = vec![codex_missing_no_pm()];
        a.handle_homebrew_install(false);
        assert!(!a.brew_install_armed, "off-macOS never arms");
        assert_eq!(count.load(SeqCst), 0, "off-macOS never opens Terminal");
        let m = a.message.unwrap_or_default();
        assert!(m.contains("only on macOS"), "footer: {m}");
    }

    #[test]
    fn homebrew_b_open_error_surfaces_in_footer() {
        use std::sync::atomic::Ordering::SeqCst;
        let (opener, count) = fake_homebrew_opener(Err("Terminal not found".into()));
        let mut a = test_app(&[]);
        a.homebrew_opener = opener;
        a.tab = Tab::Update;
        a.cli_checks = vec![codex_missing_no_pm()];
        a.handle_homebrew_install(true); // arm
        a.handle_homebrew_install(true); // open → Err
        assert_eq!(count.load(SeqCst), 1);
        assert!(!a.brew_install_armed, "disarmed even on error");
        let m = a.message.unwrap_or_default();
        assert!(m.contains("could not open Terminal"), "footer: {m}");
        assert!(
            m.contains("Terminal not found"),
            "error detail surfaced: {m}"
        );
    }

    #[test]
    fn homebrew_arm_cleared_by_intervening_non_b_key() {
        use std::sync::atomic::Ordering::SeqCst;
        let (opener, count) = fake_homebrew_opener(Ok(()));
        let mut a = test_app(&[]);
        a.homebrew_opener = opener;
        a.tab = Tab::Update;
        a.cli_checks = vec![codex_missing_no_pm()];
        a.brew_install_armed = true;
        // Any non-`b` key cancels the pending confirm (top-of-handle_key guard).
        handle_key(&mut a, KeyCode::Down);
        assert!(!a.brew_install_armed, "intervening key cancels the arm");
        // The opener was never invoked by a navigation key.
        assert_eq!(count.load(SeqCst), 0);
    }
}
