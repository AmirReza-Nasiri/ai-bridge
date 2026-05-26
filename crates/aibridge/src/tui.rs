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
use aibridge_core::{progress, review_mcp, skills};

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
    Mcp,
    Skills,
    Update,
}

impl Tab {
    const ALL: [Tab; 5] = [Tab::Health, Tab::Review, Tab::Mcp, Tab::Skills, Tab::Update];
    fn title(self) -> &'static str {
        match self {
            Tab::Health => "Health",
            Tab::Review => "Review",
            Tab::Mcp => "Codex MCP",
            Tab::Skills => "Skills",
            Tab::Update => "Update",
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
    update_rx: Option<std::sync::mpsc::Receiver<String>>,
    update_on_exit: bool,
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
            message: None,
            quit: false,
        };
        app.refresh_all();
        app
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

    /// Start a background update CHECK (read-only, networked) so the event loop never
    /// blocks. Idempotent while one is in flight.
    fn start_update_check(&mut self) {
        if self.update_rx.is_some() {
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        self.update_rx = Some(rx);
        self.update_line = "Checking for a newer release...".to_string();
        std::thread::spawn(move || {
            let _ = tx.send(aibridge_core::update::check_report(Duration::from_secs(15)));
        });
    }

    /// Poll the in-flight check; fold its result into the display line when ready.
    fn poll_update(&mut self) {
        if let Some(rx) = &self.update_rx {
            if let Ok(msg) = rx.try_recv() {
                self.update_line = msg;
                self.update_rx = None;
            }
        }
    }

    /// Full refresh (startup + `r`): runs the doctor checks (no network), reloads the
    /// review status, and re-reads the MCP policy. Not called on the fast input tick.
    fn refresh_all(&mut self) {
        self.checks = doctor::run(std::path::Path::new(&self.cwd), false, false).checks;
        self.refresh_review();
        self.refresh_mcp();
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
            Tab::Health => self.health_scroll = self.health_scroll.saturating_add(1),
            Tab::Skills => {
                if !self.managed_rows.is_empty() && self.managed_sel + 1 < self.managed_rows.len() {
                    self.managed_sel += 1;
                }
            }
            Tab::Review | Tab::Update => {}
        }
    }
    fn move_up(&mut self) {
        match self.tab {
            Tab::Mcp => match self.mcp_view {
                McpView::Servers => self.mcp_sel = self.mcp_sel.saturating_sub(1),
                McpView::Tools => self.tool_sel = self.tool_sel.saturating_sub(1),
            },
            Tab::Health => self.health_scroll = self.health_scroll.saturating_sub(1),
            Tab::Skills => self.managed_sel = self.managed_sel.saturating_sub(1),
            Tab::Review | Tab::Update => {}
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
        println!("AI Bridge {}\n", aibridge_core::VERSION_FULL);
        match aibridge_core::update::apply_update(aibridge_core::update::ApplyOptions {
            assume_yes: true,
            from_source: false,
            target_path: None,
        }) {
            Ok(m) => println!("{m}"),
            Err(e) => eprintln!("AI Bridge update: {e}"),
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
        app.poll_managed_apply(); // fold in a finished background managed-skills apply
        app.poll_check_upstream(); // fold in the upstream probe result
        app.poll_bump(); // fold in a finished bump prepare/commit
        if app.tab == Tab::Skills && app.skills_report.is_none() {
            app.refresh_skills(); // lazy first compute (folder digests) on first view
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
            let backed_out = app.mcp_back();
            if !backed_out {
                app.quit = true;
            }
        }
        KeyCode::Tab | KeyCode::Right => app.next_tab(),
        KeyCode::BackTab | KeyCode::Left => app.prev_tab(),
        KeyCode::Down | KeyCode::Char('j') => app.move_down(),
        KeyCode::Up | KeyCode::Char('k') => app.move_up(),
        // Space toggles: a tool in the per-tool view, else the selected server.
        KeyCode::Char(' ') if in_tools => app.toggle_selected_tool(),
        KeyCode::Char(' ') => app.toggle_selected_mcp(),
        // Enter opens a server's tools (server list) or toggles a tool (tool view).
        KeyCode::Enter if app.tab == Tab::Mcp && app.mcp_view == McpView::Servers => {
            app.open_selected_server()
        }
        KeyCode::Enter if in_tools => app.toggle_selected_tool(),
        KeyCode::Char('a') if in_tools => app.set_all_tools_in_view(true),
        KeyCode::Char('n') if in_tools => app.set_all_tools_in_view(false),
        KeyCode::Char('d') if in_tools => app.start_discover(),
        KeyCode::Char('c') if app.tab == Tab::Update => app.start_update_check(),
        KeyCode::Char('u') if app.tab == Tab::Update => {
            // The actual self-replace runs after the TUI exits (clean terminal + real
            // output), so it can't corrupt the alternate screen or the running binary.
            app.update_on_exit = true;
            app.quit = true;
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
        Tab::Mcp => render_mcp(f, app, rows[1]),
        Tab::Skills => render_skills(f, app, rows[1]),
        Tab::Update => render_update(f, app, rows[1]),
    }

    let help = match app.tab {
        Tab::Mcp if app.mcp_view == McpView::Tools => {
            "Up/Down: tool | Space/Enter: toggle | a: all | n: none | d: discover | Esc: back | q: quit"
        }
        Tab::Mcp => {
            "Tab/Left/Right: tabs | Up/Down: server | Space: on/off | Enter: per-tool | r: refresh | q: quit"
        }
        Tab::Health => "Tab/Left/Right: tabs | Up/Down: scroll | r: refresh | q: quit",
        Tab::Skills => {
            "Up/Dn: select | Enter: install | M: migrate-and-install | U: check upstream | B: bump (preview→commit) | p: repair | o: adopt | d: disable | x: remove | i: apply all | n: init | s/m: personal sync/migrate | r: refresh | q: quit"
        }
        Tab::Review => "Tab/Left/Right: tabs | r: refresh | q: quit (auto-refreshes ~1s)",
        Tab::Update => {
            "Tab/Left/Right: tabs | c: check | u: update now (exits + applies) | q: quit"
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

fn render_update(f: &mut Frame, app: &App, area: Rect) {
    let lines = vec![
        Line::from(format!("Installed: {}", aibridge_core::VERSION_FULL)),
        Line::from(""),
        Line::from(app.update_line.clone()),
        Line::from(""),
        Line::from(Span::styled(
            "'c' checks the latest GitHub release (read-only). 'u' downloads + verifies + \
             replaces this binary, then exits — restart aibridge afterwards.",
            Style::default().fg(Color::DarkGray),
        )),
    ];
    let p = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Update  (aibridge update)"),
        )
        .wrap(Wrap { trim: true });
    f.render_widget(p, area);
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
            message: None,
            quit: false,
        }
    }

    #[test]
    fn tab_navigation_wraps() {
        let mut a = test_app(&[]);
        assert!(a.tab == Tab::Health);
        a.next_tab();
        assert!(a.tab == Tab::Review);
        a.next_tab();
        assert!(a.tab == Tab::Mcp);
        a.next_tab();
        assert!(a.tab == Tab::Skills);
        a.next_tab();
        assert!(a.tab == Tab::Update);
        a.next_tab();
        assert!(a.tab == Tab::Health); // wrap
        a.prev_tab();
        assert!(a.tab == Tab::Update); // wrap back
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
