//! `aibridge tui` — one interactive terminal dashboard consolidating the three
//! things you'd otherwise run separately: `doctor` (Health), `status` (live Review),
//! and `review-mcp` (toggle which codex MCP servers run during reviews). ratatui +
//! crossterm. The TUI is a SEPARATE process from the MCP server, so it only READS the
//! engine's status files (no shared-state races); toggling writes review-mcp.json.
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
}

impl Tab {
    const ALL: [Tab; 3] = [Tab::Health, Tab::Review, Tab::Mcp];
    fn title(self) -> &'static str {
        match self {
            Tab::Health => "Health",
            Tab::Review => "Review",
            Tab::Mcp => "Codex MCP",
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
            message: None,
            quit: false,
        };
        app.refresh_all();
        app
    }

    /// Full refresh (startup + `r`): runs the doctor checks (no network), reloads the
    /// review status, and re-reads the MCP policy. Not called on the fast input tick.
    fn refresh_all(&mut self) {
        self.checks = doctor::run(std::path::Path::new(&self.cwd), false, false).checks;
        self.refresh_review();
        self.refresh_mcp();
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
            Tab::Mcp => {
                if let Some(rows) = &self.mcp {
                    if self.mcp_sel + 1 < rows.len() {
                        self.mcp_sel += 1;
                    }
                }
            }
            Tab::Health => self.health_scroll = self.health_scroll.saturating_add(1),
            Tab::Review => {}
        }
    }
    fn move_up(&mut self) {
        match self.tab {
            Tab::Mcp => self.mcp_sel = self.mcp_sel.saturating_sub(1),
            Tab::Health => self.health_scroll = self.health_scroll.saturating_sub(1),
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
            "AI Bridge: `tui` needs an interactive terminal. Use `aibridge doctor`, \
             `aibridge status`, or `aibridge review-mcp list` instead."
        );
        return Ok(());
    }
    let cwd = std::env::current_dir()?.to_string_lossy().to_string();
    let mut terminal = ratatui::init();
    let mut app = App::new(cwd);
    let res = run_loop(&mut terminal, &mut app);
    ratatui::restore();
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
        // Live-refresh the (cheap) review status ~1s; heavy refresh only on `r`.
        if last_refresh.elapsed() >= Duration::from_secs(1) {
            app.refresh_review();
            last_refresh = Instant::now();
        }
    }
    Ok(())
}

fn handle_key(app: &mut App, code: KeyCode) {
    match code {
        KeyCode::Char('q') | KeyCode::Esc => app.quit = true,
        KeyCode::Tab | KeyCode::Right => app.next_tab(),
        KeyCode::BackTab | KeyCode::Left => app.prev_tab(),
        KeyCode::Down | KeyCode::Char('j') => app.move_down(),
        KeyCode::Up | KeyCode::Char('k') => app.move_up(),
        KeyCode::Char(' ') | KeyCode::Enter => app.toggle_selected_mcp(),
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
    }

    let help = match app.tab {
        Tab::Mcp => {
            "Tab/Left/Right: tabs | Up/Down: select | Space/Enter: toggle | r: refresh | q: quit"
        }
        Tab::Health => "Tab/Left/Right: tabs | Up/Down: scroll | r: refresh | q: quit",
        Tab::Review => "Tab/Left/Right: tabs | r: refresh | q: quit (auto-refreshes ~1s)",
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
    let block = Block::default()
        .borders(Borders::ALL)
        .title("Codex MCP servers DURING reviews  (Space toggles; default: all off)");
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
        assert!(a.tab == Tab::Health); // wrap
        a.prev_tab();
        assert!(a.tab == Tab::Mcp); // wrap back
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
    fn movement_on_review_tab_is_noop() {
        let mut a = test_app(&["one"]);
        a.tab = Tab::Review;
        a.move_down();
        a.move_up();
        assert_eq!(a.mcp_sel, 0);
        assert_eq!(a.health_scroll, 0);
    }
}
