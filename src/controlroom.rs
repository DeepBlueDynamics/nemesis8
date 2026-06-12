//! n8 control room — what bare `n8` opens.
//!
//! A Microsoft-`edit`-style TUI: a top **menu bar** (Session / Fleet /
//! Container / Tools / Config / Help) over two **tabs** — Running (attach) and
//! Sessions (resume) — each a scrollable table showing session id + workspace.
//! Full keyboard *and* mouse (click menus/tabs/rows, wheel-scroll).
//!
//! Returns a [`PickAction`] (Attach / Resume / New) or None on quit. The menu's
//! Session items drive in-TUI actions; the other menus are the discoverability
//! outline of n8's functions (they surface the `n8 <cmd>` to run).

use anyhow::Result;
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
        MouseButton, MouseEventKind,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, Borders, Cell, Clear, Paragraph, Row, Scrollbar, ScrollbarOrientation,
        ScrollbarState, Table, TableState, Tabs,
    },
    Terminal,
};
use std::io;

use crate::picker::RunningAgent;
use crate::session::SessionInfo;

/// What the control room resolved to. Decoupled from picker::PickAction so the
/// New-session modal can carry its provider/model/danger choice straight out.
pub enum Outcome {
    Attach(String),
    Resume {
        session: SessionInfo,
        current_dir: bool,
    },
    NewSession {
        provider: String,
        model: Option<String>,
        danger: bool,
    },
}

/// Menu titles and their items. Session items (menu 0) are wired to in-TUI
/// actions by index; every other item is a discoverability hint (the text in
/// parens is the shell command to run).
// Per Law 1 (every menu item DOES something in the pane), only menus whose
// items are functional in-TUI are listed. Fleet / Container / Tools / Config
// return as real in-pane views in the next pass — they are NOT stubbed here.
const MENUS: &[(&str, &[&str])] = &[
    // Only distinct in-pane actions. Resume/Attach/List were just tab
    // navigation (the tabs already do that) — dropped. "Search sessions"
    // (all-saved content search) returns when its in-pane view ships.
    ("Session", &["New session", "Find"]),
    ("Help", &["Keys", "About"]),
];

/// Where a key hint shows: the TOP action bar or the BOTTOM nav bar. Each key
/// lives in exactly one (no top/bottom duplication).
#[derive(Clone, Copy, PartialEq)]
enum Bar {
    Top,
    Bot,
}

/// Single source of truth for key hints — drives the top action bar, the bottom
/// nav bar, AND Help ▸ Keys. Edit here and all three update. (key, what, where)
const KEYS: &[(&str, &str, Bar)] = &[
    ("n", "new", Bar::Top),
    ("⏎", "open", Bar::Top),
    ("a", "attach/resume", Bar::Top),
    (".", "resume here", Bar::Top),
    ("/", "find", Bar::Top),
    ("Tab", "Running/Sessions", Bar::Top),
    ("Alt+S", "Session", Bar::Top),
    ("Alt+H", "Help", Bar::Top),
    ("q", "quit", Bar::Top),
    ("↑↓/jk", "move", Bar::Bot),
    ("PgUp/PgDn", "page", Bar::Bot),
    ("Home/End", "ends", Bar::Bot),
];

fn bar_line(which: Bar) -> Line<'static> {
    let mut spans = vec![Span::raw(" ")];
    let mut first = true;
    for (k, what, w) in KEYS.iter() {
        if *w != which {
            continue;
        }
        if !first {
            spans.push(Span::styled(" · ", Style::default().fg(Color::DarkGray)));
        }
        first = false;
        spans.push(Span::styled(*k, Style::default().fg(Color::Yellow)));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(*what, Style::default().fg(Color::Gray)));
    }
    Line::from(spans)
}

/// A field in the New-session modal.
#[derive(Clone, Copy, PartialEq)]
enum MField {
    Provider,
    Model,
    Danger,
    Launch,
    Cancel,
}

/// New-session modal state.
struct NewModal {
    provider_idx: usize,
    model: String,
    danger: bool,
    focus: MField,
    dd_open: bool,   // provider pulldown open
    dd_sel: usize,   // highlighted provider in the pulldown
}

struct State {
    tab: usize,                 // 0 = Running, 1 = Sessions
    sel: [usize; 2],            // selected row per tab
    tstate: [TableState; 2],    // persistent so ratatui keeps scroll offset
    query: String,
    filtering: bool,
    menu_open: Option<usize>,   // which menu is dropped down
    menu_sel: usize,            // highlighted item in the open menu
    status: String,             // status-bar message (menu hints land here)
    menu_x: Vec<u16>,           // start column of each menu title (for clicks)
    detail: bool,               // detail overlay open for the selected row
    help: Option<u8>,           // Help overlay: 1 = Keys, 2 = About
    modal: Option<NewModal>,    // New-session modal
    providers: Vec<String>,     // installed providers (for the pulldown)
    dflt_provider: usize,       // default provider index when opening the modal
    dflt_model: String,
    dflt_danger: bool,
}

impl State {
    fn open_modal(&mut self) {
        self.modal = Some(NewModal {
            provider_idx: self.dflt_provider,
            model: self.dflt_model.clone(),
            danger: self.dflt_danger,
            focus: MField::Provider,
            dd_open: false,
            dd_sel: self.dflt_provider,
        });
    }
}

/// Open the control room. Returns the chosen action, or None on quit.
pub fn run(
    running: Vec<RunningAgent>,
    sessions: Vec<SessionInfo>,
    providers: Vec<String>,
    init_provider: &str,
    init_model: Option<&str>,
    init_danger: bool,
) -> Result<Option<Outcome>> {
    let providers = if providers.is_empty() {
        // Fallback: the registry's installed providers (data-driven, never a
        // hardcoded list).
        crate::provider_registry::ProviderRegistry::load()
            .names()
            .iter()
            .map(|s| s.to_string())
            .collect()
    } else {
        providers
    };
    let dflt_provider = providers
        .iter()
        .position(|p| p == init_provider)
        .unwrap_or(0);

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut st = State {
        tab: 0,
        sel: [0, 0],
        tstate: [TableState::default(), TableState::default()],
        query: String::new(),
        filtering: false,
        menu_open: None,
        menu_sel: 0,
        status: default_status(),
        menu_x: Vec::new(),
        detail: false,
        help: None,
        modal: None,
        providers,
        dflt_provider,
        dflt_model: init_model.unwrap_or("").to_string(),
        dflt_danger: init_danger,
    };

    let result = (|| -> Result<Option<Outcome>> {
        loop {
            // Filtered index lists for the active tab.
            let run_idx = filter_running(&running, &st.query);
            let sess_idx = filter_sessions(&sessions, &st.query);
            let len = if st.tab == 0 { run_idx.len() } else { sess_idx.len() };
            let last = len.saturating_sub(1);
            if st.sel[st.tab] > last {
                st.sel[st.tab] = last;
            }

            // Layout (also used for mouse hit-testing).
            let area = terminal.get_frame().area();
            let chunks = Layout::vertical([
                Constraint::Length(1), // menu bar
                Constraint::Length(1), // cheat sheet
                Constraint::Length(1), // tab strip
                Constraint::Min(1),    // table
                Constraint::Length(1), // status
            ])
            .split(area);
            let (bar_r, cheat_r, tabs_r, table_r, status_r) =
                (chunks[0], chunks[1], chunks[2], chunks[3], chunks[4]);

            // Precompute menu title x-offsets for click hit-testing.
            st.menu_x.clear();
            let mut x = bar_r.x + 1;
            for (title, _) in MENUS {
                st.menu_x.push(x);
                x += title.len() as u16 + 3; // title + padding
            }

            st.tstate[st.tab].select(Some(st.sel[st.tab]));

            terminal.draw(|f| {
                draw_bar(f, bar_r, &st);
                f.render_widget(
                    Paragraph::new(bar_line(Bar::Top)).style(Style::default().bg(Color::Indexed(235))),
                    cheat_r,
                );
                draw_tabs(f, tabs_r, &st, run_idx.len(), sess_idx.len());
                if st.tab == 0 {
                    draw_running(f, table_r, &running, &run_idx, &mut st.tstate[0]);
                } else {
                    draw_sessions(f, table_r, &sessions, &sess_idx, &mut st.tstate[1]);
                }
                draw_status(f, status_r, &st);
                if st.detail {
                    draw_detail(f, table_r, &st, &running, &run_idx, &sessions, &sess_idx);
                }
                if let Some(h) = st.help {
                    draw_help(f, f.area(), h);
                }
                if st.modal.is_some() {
                    draw_modal(f, f.area(), &st);
                }
                if let Some(mi) = st.menu_open {
                    draw_dropdown(f, bar_r, &st, mi);
                }
            })?;

            match event::read()? {
                Event::Key(k) if k.kind == KeyEventKind::Press => {
                    if let Some(action) = on_key(&mut st, k.code, k.modifiers, &running, &run_idx, &sessions, &sess_idx, last) {
                        match action {
                            Flow::Return(a) => return Ok(a),
                            Flow::Continue => {}
                        }
                    }
                }
                Event::Mouse(m) => {
                    if let Some(action) = on_mouse(&mut st, m, area, bar_r, tabs_r, table_r, last) {
                        match action {
                            Flow::Return(a) => return Ok(a),
                            Flow::Continue => {}
                        }
                    }
                }
                _ => {}
            }
        }
    })();

    disable_raw_mode().ok();
    execute!(terminal.backend_mut(), LeaveAlternateScreen, DisableMouseCapture).ok();
    terminal.show_cursor().ok();
    result
}

enum Flow {
    Return(Option<Outcome>),
    Continue,
}

fn default_status() -> String {
    String::new()
}

// ── filtering ───────────────────────────────────────────────────────────────

fn filter_running(running: &[RunningAgent], q: &str) -> Vec<usize> {
    let ql = q.to_lowercase();
    running
        .iter()
        .enumerate()
        .filter(|(_, r)| {
            q.is_empty()
                || r.name.to_lowercase().contains(&ql)
                || r.provider.to_lowercase().contains(&ql)
                || r.last_log.to_lowercase().contains(&ql)
        })
        .map(|(i, _)| i)
        .collect()
}

fn filter_sessions(sessions: &[SessionInfo], q: &str) -> Vec<usize> {
    let ql = q.to_lowercase();
    sessions
        .iter()
        .enumerate()
        .filter(|(_, s)| {
            q.is_empty()
                || s.id.to_lowercase().contains(&ql)
                || s.provider.as_deref().unwrap_or("").to_lowercase().contains(&ql)
                || s.workspace.as_deref().unwrap_or("").to_lowercase().contains(&ql)
        })
        .map(|(i, _)| i)
        .collect()
}

// ── rendering ─────────────────────────────────────────────────────────────

fn draw_bar(f: &mut ratatui::Frame, r: Rect, st: &State) {
    let mut spans = vec![Span::raw(" ")];
    for (i, (title, _)) in MENUS.iter().enumerate() {
        let active = st.menu_open == Some(i);
        let style = if active {
            Style::default().bg(Color::Indexed(238)).fg(Color::White).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Cyan)
        };
        spans.push(Span::styled(format!(" {title} "), style));
        spans.push(Span::raw(" "));
    }
    f.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(Color::Indexed(236))),
        r,
    );
    let label = " n8 control room ";
    if r.width as usize > label.len() {
        let lx = r.x + r.width - label.len() as u16 - 1;
        f.render_widget(
            Paragraph::new(Span::styled(label, Style::default().fg(Color::DarkGray))),
            Rect::new(lx, r.y, label.len() as u16, 1),
        );
    }
}

fn draw_tabs(f: &mut ratatui::Frame, r: Rect, st: &State, n_run: usize, n_sess: usize) {
    let titles = vec![
        Line::from(format!(" Running ({n_run}) ")),
        Line::from(format!(" Sessions ({n_sess}) ")),
    ];
    let tabs = Tabs::new(titles)
        .select(st.tab)
        .highlight_style(Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD))
        .divider("");
    f.render_widget(tabs, r);
}

fn draw_running(
    f: &mut ratatui::Frame,
    r: Rect,
    running: &[RunningAgent],
    idx: &[usize],
    state: &mut TableState,
) {
    let header = Row::new(["NAME", "PROV", "SESSION ID", "UPTIME", "WORKSPACE"])
        .style(Style::default().fg(Color::Indexed(244)).add_modifier(Modifier::BOLD));
    let rows: Vec<Row> = idx
        .iter()
        .map(|&i| {
            let a = &running[i];
            let sid: String = a
                .session_id
                .as_deref()
                .map(|s| s.chars().take(13).collect())
                .unwrap_or_else(|| "—".into());
            Row::new([
                Cell::from(a.name.clone()).style(Style::default().fg(Color::Cyan)),
                Cell::from(a.provider.chars().take(12).collect::<String>())
                    .style(Style::default().fg(Color::Green)),
                Cell::from(sid),
                Cell::from(a.uptime.clone()).style(Style::default().fg(Color::Gray)),
                Cell::from(a.workspace.clone().unwrap_or_else(|| "—".into()))
                    .style(Style::default().fg(Color::DarkGray)),
            ])
        })
        .collect();
    let widths = [
        Constraint::Length(16),
        Constraint::Length(12),
        Constraint::Length(14),
        Constraint::Length(12),
        Constraint::Min(10),
    ];
    render_table(f, r, header, rows, idx.len(), widths, state, "Running — ⏎ detail · a attach");
}

fn draw_sessions(
    f: &mut ratatui::Frame,
    r: Rect,
    sessions: &[SessionInfo],
    idx: &[usize],
    state: &mut TableState,
) {
    let header = Row::new(["SESSION ID", "PROV", "MODIFIED", "WORKSPACE"])
        .style(Style::default().fg(Color::Indexed(244)).add_modifier(Modifier::BOLD));
    let rows: Vec<Row> = idx
        .iter()
        .map(|&i| {
            let s = &sessions[i];
            let id: String = s.id.chars().take(13).collect();
            let modified: String = s.modified.as_deref().unwrap_or("").chars().take(16).collect();
            Row::new([
                Cell::from(id).style(Style::default().fg(Color::Cyan)),
                Cell::from(s.provider.clone().unwrap_or_else(|| "-".into()))
                    .style(Style::default().fg(Color::Green)),
                Cell::from(modified).style(Style::default().fg(Color::Gray)),
                Cell::from(s.workspace.clone().unwrap_or_default())
                    .style(Style::default().fg(Color::DarkGray)),
            ])
        })
        .collect();
    let widths = [
        Constraint::Length(14),
        Constraint::Length(10),
        Constraint::Length(18),
        Constraint::Min(10),
    ];
    render_table(f, r, header, rows, idx.len(), widths, state, "Sessions — ⏎ resume (Ctrl+⏎/. = here)");
}

#[allow(clippy::too_many_arguments)]
fn render_table(
    f: &mut ratatui::Frame,
    r: Rect,
    header: Row,
    rows: Vec<Row>,
    total: usize,
    widths: impl IntoIterator<Item = Constraint>,
    state: &mut TableState,
    title: &str,
) {
    let table = Table::new(rows, widths)
        .header(header)
        .block(Block::default().borders(Borders::ALL).title(format!("  {title}  ")))
        .row_highlight_style(
            Style::default().bg(Color::Indexed(238)).fg(Color::White).add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");
    f.render_stateful_widget(table, r, state);

    // Scrollbar reflecting offset/total.
    if total > r.height.saturating_sub(3) as usize {
        let mut sb = ScrollbarState::new(total).position(state.offset());
        f.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight).begin_symbol(None).end_symbol(None),
            r,
            &mut sb,
        );
    }
}

fn draw_status(f: &mut ratatui::Frame, r: Rect, st: &State) {
    // Bottom bar = navigation only, colorized. While filtering, show the input;
    // a transient `status` message (if any) takes priority.
    let line = if st.filtering {
        Line::from(vec![
            Span::styled(" find: ", Style::default().fg(Color::Yellow)),
            Span::styled(format!("{}▏", st.query), Style::default().fg(Color::White)),
            Span::styled("   esc clears", Style::default().fg(Color::DarkGray)),
        ])
    } else if !st.status.is_empty() {
        Line::from(Span::styled(format!(" {}", st.status), Style::default().fg(Color::Gray)))
    } else {
        bar_line(Bar::Bot)
    };
    f.render_widget(
        Paragraph::new(line).style(Style::default().bg(Color::Indexed(236))),
        r,
    );
}

fn draw_dropdown(f: &mut ratatui::Frame, bar: Rect, st: &State, mi: usize) {
    let items = MENUS[mi].1;
    let x = *st.menu_x.get(mi).unwrap_or(&bar.x);
    let w = items.iter().map(|s| s.len()).max().unwrap_or(8) as u16 + 4;
    let h = items.len() as u16 + 2;
    let dr = Rect::new(x, bar.y + 1, w.min(bar.width.saturating_sub(x - bar.x)), h);
    let lines: Vec<Line> = items
        .iter()
        .enumerate()
        .map(|(i, it)| {
            let sel = i == st.menu_sel;
            let style = if sel {
                Style::default().bg(Color::Indexed(238)).fg(Color::White).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Gray)
            };
            Line::from(Span::styled(format!(" {it} "), style))
        })
        .collect();
    f.render_widget(Clear, dr);
    f.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL)),
        dr,
    );
}

fn kv(key: &str, val: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{key:>11}: "), Style::default().fg(Color::Indexed(244))),
        Span::styled(val.to_string(), Style::default().fg(Color::White)),
    ])
}

fn centered(parent: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(parent.width);
    let h = h.min(parent.height);
    Rect::new(
        parent.x + parent.width.saturating_sub(w) / 2,
        parent.y + parent.height.saturating_sub(h) / 2,
        w,
        h,
    )
}

/// Detail overlay for the highlighted Running/Sessions row — the expand view.
fn draw_detail(
    f: &mut ratatui::Frame,
    r: Rect,
    st: &State,
    running: &[RunningAgent],
    run_idx: &[usize],
    sessions: &[SessionInfo],
    sess_idx: &[usize],
) {
    let yellow = Style::default().fg(Color::Yellow);
    let lines: Vec<Line> = if st.tab == 0 {
        match run_idx.get(st.sel[0]).map(|&i| &running[i]) {
            Some(a) => vec![
                kv("name", &a.name),
                kv("provider", &a.provider),
                kv("session id", a.session_id.as_deref().unwrap_or("—")),
                kv("uptime", &a.uptime),
                kv("workspace", a.workspace.as_deref().unwrap_or("—")),
                Line::from(""),
                Line::from(Span::styled("last activity:", Style::default().add_modifier(Modifier::BOLD))),
                Line::from(a.last_log.clone()),
                Line::from(""),
                Line::from(Span::styled("⏎/a attach · esc back", yellow)),
            ],
            None => vec![Line::from("no selection")],
        }
    } else {
        match sess_idx.get(st.sel[1]).map(|&i| &sessions[i]) {
            Some(s) => vec![
                kv("session id", &s.id),
                kv("provider", s.provider.as_deref().unwrap_or("-")),
                kv("modified", s.modified.as_deref().unwrap_or("")),
                kv("workspace", s.workspace.as_deref().unwrap_or("—")),
                kv("path", &s.path),
                Line::from(""),
                Line::from(Span::styled("⏎/a resume · . resume here · esc back", yellow)),
            ],
            None => vec![Line::from("no selection")],
        }
    };
    let h = (lines.len() as u16 + 2).min(r.height);
    let dr = centered(r, 84.min(r.width.saturating_sub(2)), h);
    f.render_widget(Clear, dr);
    f.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title("  detail  ")),
        dr,
    );
}

/// Help overlay — Keys (rendered from the KEYS registry) or About.
fn draw_help(f: &mut ratatui::Frame, area: Rect, kind: u8) {
    let lines: Vec<Line> = if kind == 1 {
        let mut v = vec![Line::from(Span::styled(
            "Keys",
            Style::default().add_modifier(Modifier::BOLD),
        ))];
        for (k, what, _) in KEYS {
            v.push(Line::from(vec![
                Span::styled(format!("{k:>10}  "), Style::default().fg(Color::Yellow)),
                Span::raw(*what),
            ]));
        }
        v.push(Line::from(""));
        v.push(Line::from(Span::styled("esc/q close", Style::default().fg(Color::Gray))));
        v
    } else {
        vec![
            Line::from(Span::styled(
                "nemesis8 control room",
                Style::default().add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from("Run AI agents in Docker. Bare `n8` opens this."),
            Line::from(format!("version {}", env!("CARGO_PKG_VERSION"))),
            Line::from(""),
            Line::from(Span::styled("esc/q close", Style::default().fg(Color::Gray))),
        ]
    };
    let w = 52.min(area.width.saturating_sub(2));
    let h = (lines.len() as u16 + 2).min(area.height.saturating_sub(1));
    let dr = centered(area, w, h);
    f.render_widget(Clear, dr);
    f.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title("  Help  ")),
        dr,
    );
}

// ── new-session modal ────────────────────────────────────────────────────────

fn next_field(f: MField) -> MField {
    match f {
        MField::Provider => MField::Model,
        MField::Model => MField::Danger,
        MField::Danger => MField::Launch,
        MField::Launch => MField::Cancel,
        MField::Cancel => MField::Provider,
    }
}
fn prev_field(f: MField) -> MField {
    match f {
        MField::Provider => MField::Cancel,
        MField::Model => MField::Provider,
        MField::Danger => MField::Model,
        MField::Launch => MField::Danger,
        MField::Cancel => MField::Launch,
    }
}

/// Take the modal's choices and return the launch outcome.
fn confirm_modal(st: &mut State) -> Flow {
    if let Some(m) = st.modal.take() {
        let provider = st
            .providers
            .get(m.provider_idx)
            .or(st.providers.first())
            .cloned()
            .unwrap_or_default();
        let t = m.model.trim();
        let model = if t.is_empty() { None } else { Some(t.to_string()) };
        return Flow::Return(Some(Outcome::NewSession {
            provider,
            model,
            danger: m.danger,
        }));
    }
    Flow::Continue
}

fn hit(r: Rect, col: u16, row: u16) -> bool {
    col >= r.x && col < r.x + r.width && row >= r.y && row < r.y + r.height
}
fn hit_col(r: Rect, col: u16) -> bool {
    col >= r.x && col < r.x + r.width
}

/// Modal layout rects: (modal, provider, model, danger, launch, cancel).
fn modal_rects(area: Rect) -> (Rect, Rect, Rect, Rect, Rect, Rect) {
    let modal = centered(
        area,
        58.min(area.width.saturating_sub(2)),
        9.min(area.height.saturating_sub(2)),
    );
    let ix = modal.x + 2;
    let iw = modal.width.saturating_sub(4);
    (
        modal,
        Rect::new(ix, modal.y + 2, iw, 1),
        Rect::new(ix, modal.y + 3, iw, 1),
        Rect::new(ix, modal.y + 4, iw, 1),
        Rect::new(ix, modal.y + 6, 10, 1),
        Rect::new(ix + 12, modal.y + 6, 10, 1),
    )
}

fn draw_modal(f: &mut ratatui::Frame, area: Rect, st: &State) {
    let Some(m) = st.modal.as_ref() else { return };
    let (modal, pr, mr, dr, lb, cb) = modal_rects(area);
    f.render_widget(Clear, modal);
    f.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan))
            .title("  New session  "),
        modal,
    );
    let fld = |label: &str, value: String, focused: bool| -> Line<'static> {
        Line::from(vec![
            Span::styled(format!("{label:<9} "), Style::default().fg(Color::Indexed(244))),
            Span::styled(
                value,
                if focused {
                    Style::default().fg(Color::White).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::Gray)
                },
            ),
        ])
    };
    let prov = st.providers.get(m.provider_idx).cloned().unwrap_or_default();
    f.render_widget(
        Paragraph::new(fld("Provider", format!("[ {prov}  ▾ ]"), m.focus == MField::Provider)),
        pr,
    );
    let modelval = if m.model.is_empty() {
        "[ default ]".to_string()
    } else {
        format!("[ {}▏ ]", m.model)
    };
    f.render_widget(Paragraph::new(fld("Model", modelval, m.focus == MField::Model)), mr);
    f.render_widget(
        Paragraph::new(fld(
            "Danger",
            format!("[{}] skip approvals + sandbox", if m.danger { "x" } else { " " }),
            m.focus == MField::Danger,
        )),
        dr,
    );
    let btn = |label: &str, focused: bool| -> Paragraph<'static> {
        Paragraph::new(Span::styled(
            format!(" {label} "),
            if focused {
                Style::default().bg(Color::Cyan).fg(Color::Black).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Yellow)
            },
        ))
    };
    f.render_widget(btn("Launch", m.focus == MField::Launch), lb);
    f.render_widget(btn("Cancel", m.focus == MField::Cancel), cb);

    // Provider pulldown (rendered last so it sits above the model row).
    if m.dd_open {
        let np = st.providers.len() as u16;
        let h = (np + 2).min(area.height.saturating_sub(pr.y + 1));
        let dd = Rect::new(pr.x, pr.y + 1, pr.width.clamp(12, 30), h);
        f.render_widget(Clear, dd);
        let lines: Vec<Line> = st
            .providers
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let sel = i == m.dd_sel;
                Line::from(Span::styled(
                    format!(" {p} "),
                    if sel {
                        Style::default().bg(Color::Indexed(238)).fg(Color::White)
                    } else {
                        Style::default().fg(Color::Gray)
                    },
                ))
            })
            .collect();
        f.render_widget(
            Paragraph::new(lines).block(Block::default().borders(Borders::ALL)),
            dd,
        );
    }
}

// ── input ───────────────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn on_key(
    st: &mut State,
    code: KeyCode,
    mods: KeyModifiers,
    running: &[RunningAgent],
    run_idx: &[usize],
    sessions: &[SessionInfo],
    sess_idx: &[usize],
    last: usize,
) -> Option<Flow> {
    // New-session modal swallows all keys until closed/launched.
    if st.modal.is_some() {
        let np = st.providers.len().max(1);
        let mut confirm = false;
        let mut close = false;
        {
            let m = st.modal.as_mut().unwrap();
            if m.dd_open {
                match code {
                    KeyCode::Esc => m.dd_open = false,
                    KeyCode::Up => m.dd_sel = (m.dd_sel + np - 1) % np,
                    KeyCode::Down => m.dd_sel = (m.dd_sel + 1) % np,
                    KeyCode::Enter => {
                        m.provider_idx = m.dd_sel;
                        m.dd_open = false;
                    }
                    _ => {}
                }
                return Some(Flow::Continue);
            }
            match code {
                KeyCode::Esc => close = true,
                KeyCode::Tab | KeyCode::Down => m.focus = next_field(m.focus),
                KeyCode::BackTab | KeyCode::Up => m.focus = prev_field(m.focus),
                KeyCode::Left | KeyCode::Right => match m.focus {
                    MField::Provider => {
                        m.provider_idx = if matches!(code, KeyCode::Left) {
                            (m.provider_idx + np - 1) % np
                        } else {
                            (m.provider_idx + 1) % np
                        };
                    }
                    MField::Danger => m.danger = !m.danger,
                    MField::Launch => m.focus = MField::Cancel,
                    MField::Cancel => m.focus = MField::Launch,
                    MField::Model => {}
                },
                KeyCode::Char(' ') if m.focus == MField::Danger => m.danger = !m.danger,
                KeyCode::Char(c) if m.focus == MField::Model => m.model.push(c),
                KeyCode::Backspace if m.focus == MField::Model => {
                    m.model.pop();
                }
                KeyCode::Enter => match m.focus {
                    MField::Provider => {
                        m.dd_open = true;
                        m.dd_sel = m.provider_idx;
                    }
                    MField::Cancel => close = true,
                    _ => confirm = true,
                },
                _ => {}
            }
        }
        if confirm {
            return Some(confirm_modal(st));
        }
        if close {
            st.modal = None;
        }
        return Some(Flow::Continue);
    }

    // Help overlay swallows keys until closed.
    if st.help.is_some() {
        if matches!(code, KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter) {
            st.help = None;
        }
        return Some(Flow::Continue);
    }
    // Detail overlay: act on the selected row, or close.
    if st.detail {
        match code {
            KeyCode::Esc => st.detail = false,
            KeyCode::Char('a') | KeyCode::Enter => {
                return Some(activate(st, running, run_idx, sessions, sess_idx, false))
            }
            KeyCode::Char('.') => {
                return Some(activate(st, running, run_idx, sessions, sess_idx, true))
            }
            _ => {}
        }
        return Some(Flow::Continue);
    }

    // Menu navigation takes priority when a dropdown is open.
    if let Some(mi) = st.menu_open {
        let n = MENUS[mi].1.len();
        match code {
            KeyCode::Esc => st.menu_open = None,
            KeyCode::Left => { st.menu_open = Some((mi + MENUS.len() - 1) % MENUS.len()); st.menu_sel = 0; }
            KeyCode::Right => { st.menu_open = Some((mi + 1) % MENUS.len()); st.menu_sel = 0; }
            KeyCode::Up => st.menu_sel = (st.menu_sel + n - 1) % n,
            KeyCode::Down => st.menu_sel = (st.menu_sel + 1) % n,
            KeyCode::Enter => return Some(menu_select(st, mi, st.menu_sel)),
            _ => {}
        }
        return Some(Flow::Continue);
    }

    // Filter editing.
    if st.filtering {
        match code {
            KeyCode::Esc => { st.filtering = false; st.query.clear(); st.sel[st.tab] = 0; }
            KeyCode::Backspace => { st.query.pop(); st.sel[st.tab] = 0; }
            KeyCode::Char(c) => { st.query.push(c); st.sel[st.tab] = 0; }
            KeyCode::Enter => return Some(activate(st, running, run_idx, sessions, sess_idx, false)),
            KeyCode::Up | KeyCode::Down => {} // fallthrough below
            _ => return Some(Flow::Continue),
        }
        if !matches!(code, KeyCode::Up | KeyCode::Down) {
            return Some(Flow::Continue);
        }
    }

    // Alt+letter opens a menu.
    if mods.contains(KeyModifiers::ALT) {
        if let KeyCode::Char(c) = code {
            if let Some(i) = MENUS.iter().position(|(t, _)| t.to_lowercase().starts_with(c.to_ascii_lowercase())) {
                st.menu_open = Some(i);
                st.menu_sel = 0;
                return Some(Flow::Continue);
            }
        }
    }

    match code {
        KeyCode::Char('q') | KeyCode::Esc => return Some(Flow::Return(None)),
        KeyCode::Char('n') => st.open_modal(),
        KeyCode::Char('/') => st.filtering = true,
        KeyCode::Tab | KeyCode::BackTab => st.tab = 1 - st.tab,
        KeyCode::Char('1') => st.tab = 0,
        KeyCode::Char('2') => st.tab = 1,
        KeyCode::Up | KeyCode::Char('k') => st.sel[st.tab] = st.sel[st.tab].saturating_sub(1),
        KeyCode::Down | KeyCode::Char('j') => { if st.sel[st.tab] < last { st.sel[st.tab] += 1; } }
        KeyCode::PageUp => st.sel[st.tab] = st.sel[st.tab].saturating_sub(10),
        KeyCode::PageDown => st.sel[st.tab] = (st.sel[st.tab] + 10).min(last),
        KeyCode::Home | KeyCode::Char('g') => st.sel[st.tab] = 0,
        KeyCode::End | KeyCode::Char('G') => st.sel[st.tab] = last,
        KeyCode::Char('a') => return Some(activate(st, running, run_idx, sessions, sess_idx, false)),
        KeyCode::Char('.') => return Some(activate(st, running, run_idx, sessions, sess_idx, true)),
        KeyCode::Enter => st.detail = true,
        _ => {}
    }
    Some(Flow::Continue)
}

/// Turn the highlighted row into an action. `current_dir` only affects resume.
fn activate(
    st: &State,
    running: &[RunningAgent],
    run_idx: &[usize],
    sessions: &[SessionInfo],
    sess_idx: &[usize],
    current_dir: bool,
) -> Flow {
    if st.tab == 0 {
        if let Some(&i) = run_idx.get(st.sel[0]) {
            return Flow::Return(Some(Outcome::Attach(running[i].name.clone())));
        }
    } else if let Some(&j) = sess_idx.get(st.sel[1]) {
        return Flow::Return(Some(Outcome::Resume {
            session: sessions[j].clone(),
            current_dir,
        }));
    }
    Flow::Continue
}

/// Handle a menu item selection.
fn menu_select(st: &mut State, menu: usize, item: usize) -> Flow {
    st.menu_open = None;
    match menu {
        0 => match item {
            // Session
            0 => st.open_modal(), // New session → modal
            1 => st.filtering = true, // Find
            _ => {}
        },
        1 => st.help = Some(if item == 0 { 1 } else { 2 }), // Help: Keys / About
        _ => {}
    }
    Flow::Continue
}

fn on_mouse(
    st: &mut State,
    m: event::MouseEvent,
    area: Rect,
    bar_r: Rect,
    tabs_r: Rect,
    table_r: Rect,
    last: usize,
) -> Option<Flow> {
    let (col, row) = (m.column, m.row);
    // Modal grabs the mouse first.
    if st.modal.is_some() {
        if let MouseEventKind::Down(MouseButton::Left) = m.kind {
            let (modal, pr, mr, dr, lb, cb) = modal_rects(area);
            let dd_open = st.modal.as_ref().map(|x| x.dd_open).unwrap_or(false);
            if dd_open {
                // Dropdown is a bordered block at pr.y+1, so the first item
                // renders one row down (inside the top border) at pr.y+2.
                let top = pr.y + 2;
                let np = st.providers.len();
                if row >= top && (row as usize) < top as usize + np && hit_col(pr, col) {
                    let i = (row - top) as usize;
                    if let Some(mm) = st.modal.as_mut() {
                        mm.provider_idx = i;
                        mm.dd_open = false;
                    }
                } else if let Some(mm) = st.modal.as_mut() {
                    mm.dd_open = false;
                }
            } else if hit(pr, col, row) {
                if let Some(mm) = st.modal.as_mut() {
                    mm.focus = MField::Provider;
                    mm.dd_open = true;
                    mm.dd_sel = mm.provider_idx;
                }
            } else if hit(mr, col, row) {
                if let Some(mm) = st.modal.as_mut() {
                    mm.focus = MField::Model;
                }
            } else if hit(dr, col, row) {
                if let Some(mm) = st.modal.as_mut() {
                    mm.danger = !mm.danger;
                    mm.focus = MField::Danger;
                }
            } else if hit(lb, col, row) {
                return Some(confirm_modal(st));
            } else if hit(cb, col, row) || !hit(modal, col, row) {
                st.modal = None;
            }
        }
        return Some(Flow::Continue);
    }
    // Overlays grab the mouse: a click dismisses them.
    if st.help.is_some() {
        if matches!(m.kind, MouseEventKind::Down(_)) {
            st.help = None;
        }
        return Some(Flow::Continue);
    }
    if st.detail {
        if matches!(m.kind, MouseEventKind::Down(_)) {
            st.detail = false;
        }
        return Some(Flow::Continue);
    }
    match m.kind {
        MouseEventKind::ScrollDown => {
            st.sel[st.tab] = (st.sel[st.tab] + 3).min(last);
        }
        MouseEventKind::ScrollUp => {
            st.sel[st.tab] = st.sel[st.tab].saturating_sub(3);
        }
        MouseEventKind::Down(MouseButton::Left) => {
            // Menu bar?
            if row == bar_r.y {
                if let Some(i) = menu_at(st, col) {
                    st.menu_open = if st.menu_open == Some(i) { None } else { Some(i) };
                    st.menu_sel = 0;
                    return Some(Flow::Continue);
                }
                st.menu_open = None;
                return Some(Flow::Continue);
            }
            // Open dropdown item?
            if let Some(mi) = st.menu_open {
                let items = MENUS[mi].1;
                let x = *st.menu_x.get(mi).unwrap_or(&bar_r.x);
                let top = bar_r.y + 2; // first item row (inside the border)
                if row >= top && (row as usize) < top as usize + items.len() && col >= x {
                    return Some(menu_select(st, mi, (row - top) as usize));
                }
                st.menu_open = None;
                return Some(Flow::Continue);
            }
            // Tab strip? Running (left) vs Sessions.
            if row == tabs_r.y {
                st.tab = if (col as usize) < (tabs_r.x as usize + 12) { 0 } else { 1 };
                return Some(Flow::Continue);
            }
            // Table row? rows start at table_r.y + 2 (border + header).
            let first = table_r.y + 2;
            if row >= first && row < table_r.y + table_r.height.saturating_sub(1) {
                let visible_idx = (row - first) as usize;
                let offset = st.tstate[st.tab].offset();
                let sel = offset + visible_idx;
                if sel <= last {
                    st.sel[st.tab] = sel;
                    st.detail = true; // click a row → open its detail
                }
            }
        }
        _ => {}
    }
    Some(Flow::Continue)
}

fn menu_at(st: &State, col: u16) -> Option<usize> {
    for (i, (title, _)) in MENUS.iter().enumerate() {
        let start = *st.menu_x.get(i)?;
        let end = start + title.len() as u16 + 2;
        if col >= start && col < end {
            return Some(i);
        }
    }
    None
}
