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
use aibridge_core::{claude_mcp, progress, review_mcp, skills};

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
    Newer {
        from: String,
        to: String,
        summary: String,
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

/// A finished Claude-side discovery: (server name, tools or error). Same shape as the
/// codex `DiscoverResult` but kept separate so a late codex-tab message can't poison
/// the Claude inspector's per-tool view (and vice versa).
type ClaudeDiscoverResult = (String, Result<Vec<String>, String>);

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
    /// in-flight check (background thread → channel). The actual self-replace runs
    /// AFTER the TUI exits (clean terminal + real output), gated by `update_on_exit`.
    update_line: String,
    update_rx: Option<std::sync::mpsc::Receiver<SelfUpdateProbeResult>>,
    update_on_exit: bool,
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
    /// Mutating CLI commands queued by `u` on a CLI row; drained AFTER the TUI
    /// exits (mirrors `update_on_exit` for the aibridge self-update).
    pending_cli_updates: Vec<aibridge_core::cli_update::CliCheck>,
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

impl App {
    fn new(cwd: String) -> Self {
        let mut app = App {
            cwd,
            tab: Tab::Health,
            checks: Vec::new(),
            health_scroll: 0,
            review: None,
            review_summary: None,
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
            update_on_exit: false,
            self_update_state: SelfUpdateState::Unprobed,
            update_check_kicked: false,
            clipboard: aibridge_platform::real_clipboard(),
            planner: std::sync::Arc::new(aibridge_core::update::plan_update),
            cli_checks: Vec::new(),
            cli_checks_rx: None,
            mcp_pins: Vec::new(),
            update_sel: 0,
            pending_cli_updates: Vec::new(),
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
                        SelfUpdateState::Newer { from, to, summary }
                    }
                    Err(detail) => {
                        self.update_line = format!("Self-update check failed: {detail}");
                        SelfUpdateState::Error { detail }
                    }
                };
                self.update_rx = None;
            }
        }
        if let Some(rx) = &self.cli_checks_rx {
            if let Ok(checks) = rx.try_recv() {
                self.cli_checks = checks;
                self.cli_checks_rx = None;
            }
        }
    }

    /// Start a background CLI-checks worker (codex / claude / rtk). Read-only —
    /// no mutation. Idempotent while one is in flight.
    fn start_cli_checks(&mut self) {
        if self.cli_checks_rx.is_some() {
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        self.cli_checks_rx = Some(rx);
        std::thread::spawn(move || {
            let runner = aibridge_core::cli_update::RealCommandRunner;
            let _ = tx.send(aibridge_core::cli_update::check_all(&runner));
        });
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
            // v0.20.1 R3 B1: dispatch on the TYPED self-update state. Only Newer
            // exits the TUI; every other state shows a footer message and stays
            // in the dashboard. Fixes the v0.20.0 bug where pressing `u` while
            // already on the latest release dumped the user to the shell.
            match &self.self_update_state {
                SelfUpdateState::Newer { from, to, .. } => {
                    self.update_line = format!("Applying update: {from} → {to}...");
                    self.update_on_exit = true;
                    self.quit = true;
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
            if c.up_to_date() {
                self.message = Some(format!("{} is up to date — nothing to do.", c.tool));
                return;
            }
            // v0.20.0 R7 B3: use `safe_to_auto_run` so the trusted internal
            // `aibridge rtk install/update --yes` form is also accepted (and a
            // stale-PATH "aibridge" path is rejected).
            if c.suggested_command.is_none() || !c.safe_to_auto_run() {
                // Manual-only / unknown — show the hint, no mutation.
                self.message = Some(format!(
                    "{}: manual update — {}",
                    c.tool,
                    c.manual_note.as_deref().unwrap_or("see docs")
                ));
                return;
            }
            // Verified package-manager source: enqueue + exit TUI to run after-restore.
            self.pending_cli_updates.push(c.clone());
            self.quit = true;
            return;
        }
        // Otherwise it's an MCP-pin row — read-only.
        self.message = Some(
            "MCP version pins are read-only in this view; edit ~/.claude.json or .mcp.json to change".into(),
        );
    }

    /// Full refresh (startup + `r`): runs the doctor checks (no network), reloads the
    /// review status, and re-reads the MCP policies (codex + claude). Not called on
    /// the fast input tick.
    fn refresh_all(&mut self) {
        self.checks = doctor::run(std::path::Path::new(&self.cwd), false, false).checks;
        self.refresh_review();
        self.refresh_mcp();
        self.refresh_claude();
        if self.mcp_view == McpView::Tools {
            self.load_tool_states();
        }
    }

    /// Cheap (file read) — safe to call on the ~1s auto-refresh.
    fn refresh_review(&mut self) {
        self.review = progress::read_status(&self.cwd);
        self.review_summary = progress::status_report(&self.cwd);
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
        self.tab = Tab::ALL[(self.tab.index() + 1) % Tab::ALL.len()];
    }
    fn prev_tab(&mut self) {
        self.tab = Tab::ALL[(self.tab.index() + Tab::ALL.len() - 1) % Tab::ALL.len()];
    }
    fn move_down(&mut self) {
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
            Tab::Review => {}
        }
    }
    fn move_up(&mut self) {
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
            Tab::Review => {}
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
    // The actual update runs HERE — after the terminal is restored — so its output
    // shows normally and replacing the running binary can't corrupt the dashboard.
    // Only if the loop ended cleanly (don't self-update on top of a loop error).
    if res.is_ok() && app.update_on_exit {
        use aibridge_core::process_cleanup::{RealProcessEnumerator, RealProcessKiller};
        use aibridge_core::update::{
            apply_planned_update, orchestrate_update, plan_update, ApplyOptions, CleanupMode,
            OrchestrationOpts, RealConfirmer,
        };
        println!("AI Bridge {}\n", aibridge_core::VERSION_FULL);
        // v0.20.0 hotfix: plan_update first, then orchestrate cleanup+apply in
        // the restored terminal. User pressed `u` → update_already_confirmed=true.
        // Cleanup mode is Prompt — pressing `u` consents to UPDATE only; killing
        // other same-path aibridge processes (MCP servers, other TUIs) requires
        // a separate explicit [y/N].
        let opts = ApplyOptions {
            assume_yes: true,
            from_source: false,
            target_path: None,
        };
        match plan_update(&opts) {
            Ok(decision) => {
                let orch = OrchestrationOpts {
                    update_already_confirmed: true,
                    cleanup_mode: CleanupMode::Prompt,
                };
                match orchestrate_update(
                    decision,
                    orch,
                    &RealProcessEnumerator,
                    &RealProcessKiller,
                    &RealConfirmer,
                    &apply_planned_update,
                ) {
                    Ok(m) => println!("{m}"),
                    Err(e) => eprintln!("AI Bridge update: {e}"),
                }
            }
            Err(e) => eprintln!("AI Bridge update: {e}"),
        }
    }
    // Codex Stop-gate R6: mutating CLI commands run in the RESTORED terminal so
    // brew/npm progress + prompts are visible. Each pending row gets a default-no
    // confirmation; the user can cancel any single row without aborting the batch.
    if res.is_ok() && !app.pending_cli_updates.is_empty() {
        use aibridge_core::cli_update::{apply_cli_update, prompt_parse};
        use std::io::{BufRead, IsTerminal, Write};
        println!("\nApplying selected CLI updates:");
        let is_tty = std::io::stdin().is_terminal();
        for c in &app.pending_cli_updates {
            let Some(argv) = &c.suggested_command else {
                continue;
            };
            print!(
                "  [{tool}] run `{cmd}` ? [y/N] ",
                tool = c.tool,
                cmd = argv.join(" ")
            );
            let _ = std::io::stdout().flush();
            let accept = if is_tty {
                let mut line = String::new();
                let _ = std::io::stdin().lock().read_line(&mut line);
                prompt_parse(&line)
            } else {
                println!("(non-tty — declined)");
                false
            };
            if !accept {
                println!("    declined.");
                continue;
            }
            match apply_cli_update(argv) {
                Ok(0) => println!("    [{tool}] success.", tool = c.tool),
                Ok(code) => eprintln!("    [{tool}] exited with code {code}", tool = c.tool),
                Err(e) => eprintln!("    [{tool}] failed: {e}", tool = c.tool),
            }
        }
    }
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
    match code {
        KeyCode::Char('q') => app.quit = true,
        // Esc backs out of the per-tool view first; only quits at the top level.
        // (Bind first so the arm body isn't a lone `if` — avoids clippy
        // collapsible_match wanting a side-effecting match guard.)
        KeyCode::Esc => {
            let backed_out = app.mcp_back() || app.claude_back();
            if !backed_out {
                app.quit = true;
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
            // Mutation (any case) defers to after-TUI-exit per Codex Stop-gate R6.
            app.handle_update_action();
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
        Tab::Health => "Tab/Left/Right: tabs | Up/Down: scroll | r: refresh | q: quit",
        Tab::Skills => {
            "Up/Dn: select | Enter: install | M: migrate-and-install | U: check upstream | B: bump (preview→commit) | p: repair | o: adopt | d: disable | x: remove | i: apply all | n: init | s/m: personal sync/migrate | r: refresh | q: quit"
        }
        Tab::Review => "Tab/Left/Right: tabs | r: refresh | q: quit (auto-refreshes ~1s)",
        Tab::Update => {
            "↑/↓: select | c: check self | u: update selected row (exits + applies) | r: re-check CLIs | q: quit"
        }
        Tab::Debug => "Tab/Left/Right: tabs | Up/Down: scroll | r: rebuild | q: quit",
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
                .title("Health  (aibridge doctor)"),
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
    let p = Paragraph::new(body)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Debug  (auto-sanitized; review before sharing publicly)"),
        )
        .wrap(Wrap { trim: false })
        .scroll((app.debug_scroll, 0));
    f.render_widget(p, area);
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
        let status = if c.up_to_date() {
            format!("up-to-date  ({cur})")
        } else {
            format!("{cur} → {latest}")
        };
        let line = format!(
            "  [{tool}]  {status}  via {src}",
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
    items.push(ListItem::new(Line::from(Span::styled(
        "↑/↓ navigate · 'c' check self · 'u' update selected row · 'r' re-check CLIs",
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
            update_on_exit: false,
            self_update_state: SelfUpdateState::Unprobed,
            update_check_kicked: false,
            clipboard: aibridge_platform::real_clipboard(),
            planner: std::sync::Arc::new(aibridge_core::update::plan_update),
            cli_checks: Vec::new(),
            cli_checks_rx: None,
            mcp_pins: Vec::new(),
            update_sel: 0,
            pending_cli_updates: Vec::new(),
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
    fn update_u_on_self_row_with_newer_state_sets_update_on_exit() {
        // v0.20.1 R3 B5: Updated for the typed-state behavior. Only `Newer` quits;
        // `Unprobed` (the old default) now shows a footer message and stays in TUI.
        let mut a = test_app(&[]);
        a.tab = Tab::Update;
        a.update_sel = 0;
        a.self_update_state = SelfUpdateState::Newer {
            from: "0.20.0".into(),
            to: "0.21.0".into(),
            summary: "test".into(),
        };
        a.handle_update_action();
        assert!(a.update_on_exit);
        assert!(a.quit);
        assert!(a.pending_cli_updates.is_empty());
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
        assert!(!a.update_on_exit);
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
        }];
        a.update_sel = 1; // first CLI row
        a.handle_update_action();
        // No mutation queued, no exit triggered.
        assert!(a.pending_cli_updates.is_empty());
        assert!(!a.quit);
        // But the user gets a footer message pointing at the docs URL.
        let m = a.message.unwrap_or_default();
        assert!(
            m.contains("manual update"),
            "footer should mention manual update: {m:?}"
        );
    }

    #[test]
    fn update_u_on_cli_row_with_verified_source_enqueues_and_exits() {
        use aibridge_core::cli_update::{CliCheck, InstallSource};
        use aibridge_core::update::parse_version;
        let mut a = test_app(&[]);
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
        }];
        a.update_sel = 1;
        a.handle_update_action();
        assert!(a.quit, "should exit TUI so brew runs in restored terminal");
        assert_eq!(a.pending_cli_updates.len(), 1);
        assert_eq!(a.pending_cli_updates[0].tool, "codex");
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
        assert!(a.pending_cli_updates.is_empty());
        assert!(!a.quit);
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
}
