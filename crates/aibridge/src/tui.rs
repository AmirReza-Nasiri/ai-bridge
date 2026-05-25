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
use aibridge_core::{progress, review_mcp};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Health,
    Review,
    Mcp,
    Update,
}

impl Tab {
    const ALL: [Tab; 4] = [Tab::Health, Tab::Review, Tab::Mcp, Tab::Update];
    fn title(self) -> &'static str {
        match self {
            Tab::Health => "Health",
            Tab::Review => "Review",
            Tab::Mcp => "Codex MCP",
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
            message: None,
            quit: false,
        };
        app.refresh_all();
        app
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
        self.mcp = review_mcp::codex_server_names().map(|names| {
            let allow = review_mcp::allowlist();
            names
                .into_iter()
                .map(|name| {
                    let enabled = allow.iter().any(|a| a == &name);
                    let interactive = review_mcp::server_looks_interactive(&name);
                    McpRow {
                        name,
                        enabled,
                        interactive,
                    }
                })
                .collect()
        });
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
    match code {
        KeyCode::Char('q') => app.quit = true,
        // Esc backs out of the per-tool view first; only quits at the top level.
        KeyCode::Esc => {
            if !app.mcp_back() {
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
        KeyCode::Char('d') if in_tools => app.start_discover(),
        KeyCode::Char('c') if app.tab == Tab::Update => app.start_update_check(),
        KeyCode::Char('u') if app.tab == Tab::Update => {
            // The actual self-replace runs after the TUI exits (clean terminal + real
            // output), so it can't corrupt the alternate screen or the running binary.
            app.update_on_exit = true;
            app.quit = true;
        }
        KeyCode::Char('r') => app.refresh_all(),
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
        Tab::Update => render_update(f, app, rows[1]),
    }

    let help = match app.tab {
        Tab::Mcp if app.mcp_view == McpView::Tools => {
            "Up/Down: tool | Space/Enter: toggle | d: (re)discover | Esc: back | q: quit"
        }
        Tab::Mcp => {
            "Tab/Left/Right: tabs | Up/Down: server | Space: on/off | Enter: per-tool | r: refresh | q: quit"
        }
        Tab::Health => "Tab/Left/Right: tabs | Up/Down: scroll | r: refresh | q: quit",
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
                "WARNING: ~/.codex/config.toml is present but can't be read/parsed.\n\
                 AI Bridge will REFUSE to run reviews (fail-closed) until it's fixed.",
            )
            .style(Style::default().fg(Color::Red))
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
        "Tools of '{server}' DURING reviews  (Space/Enter: toggle; d: (re)discover; Esc: back)"
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
        assert!(a.tab == Tab::Update);
        a.next_tab();
        assert!(a.tab == Tab::Health); // wrap
        a.prev_tab();
        assert!(a.tab == Tab::Update); // wrap back
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
