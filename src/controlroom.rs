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

use crate::picker::{PickAction, RunningAgent};
use crate::session::SessionInfo;

/// Menu titles and their items. Session items (menu 0) are wired to in-TUI
/// actions by index; every other item is a discoverability hint (the text in
/// parens is the shell command to run).
const MENUS: &[(&str, &[&str])] = &[
    (
        "Session",
        &["New session", "Resume (Sessions tab)", "Attach (Running tab)", "Search", "List sessions"],
    ),
    (
        "Fleet",
        &["Agents — n8 agents", "Spawn — n8 agents spawn", "Kill — n8 agents kill", "Serve — n8 serve"],
    ),
    (
        "Container",
        &["Build — n8 build", "Running — n8 ps", "Shell — n8 shell", "Stop — n8 stop", "Pokeball — n8 pokeball"],
    ),
    ("Tools", &["MCP tools — n8 mcp", "Mounts — n8 mount"]),
    ("Config", &["Init — n8 init", "Doctor — n8 doctor", "Login — n8 login", "Update — n8 update"]),
    ("Help", &["Keys", "About"]),
];

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
}

/// Open the control room. Returns the chosen action, or None on quit.
pub fn run(running: Vec<RunningAgent>, sessions: Vec<SessionInfo>) -> Result<Option<PickAction>> {
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
    };

    let result = (|| -> Result<Option<PickAction>> {
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
                Constraint::Length(1), // tab strip
                Constraint::Min(1),    // table
                Constraint::Length(1), // status
            ])
            .split(area);
            let (bar_r, tabs_r, table_r, status_r) = (chunks[0], chunks[1], chunks[2], chunks[3]);

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
                draw_tabs(f, tabs_r, &st, run_idx.len(), sess_idx.len());
                if st.tab == 0 {
                    draw_running(f, table_r, &running, &run_idx, &mut st.tstate[0]);
                } else {
                    draw_sessions(f, table_r, &sessions, &sess_idx, &mut st.tstate[1]);
                }
                draw_status(f, status_r, &st);
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
                    if let Some(action) = on_mouse(&mut st, m, bar_r, tabs_r, table_r, last) {
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
    Return(Option<PickAction>),
    Continue,
}

fn default_status() -> String {
    "↑↓ move · ⏎ open · / filter · Tab switch · Alt+letter menu · q quit".to_string()
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
            Row::new([
                Cell::from(a.name.clone()).style(Style::default().fg(Color::Cyan)),
                Cell::from(a.provider.clone()).style(Style::default().fg(Color::Green)),
                Cell::from(a.session_id.clone().unwrap_or_else(|| "—".into())),
                Cell::from(a.uptime.clone()).style(Style::default().fg(Color::Gray)),
                Cell::from(a.last_log.chars().take(40).collect::<String>())
                    .style(Style::default().fg(Color::DarkGray)),
            ])
        })
        .collect();
    let widths = [
        Constraint::Length(16),
        Constraint::Length(10),
        Constraint::Length(14),
        Constraint::Length(10),
        Constraint::Min(10),
    ];
    render_table(f, r, header, rows, idx.len(), widths, state, "Running — ⏎ attach");
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
    let text = if st.filtering {
        format!("filter: {}▏   (esc clears)", st.query)
    } else {
        st.status.clone()
    };
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(format!(" {text}"), Style::default().fg(Color::Gray))))
            .style(Style::default().bg(Color::Indexed(236))),
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
        KeyCode::Char('.') => return Some(activate(st, running, run_idx, sessions, sess_idx, true)),
        KeyCode::Enter => {
            let current = mods.contains(KeyModifiers::CONTROL);
            return Some(activate(st, running, run_idx, sessions, sess_idx, current));
        }
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
            return Flow::Return(Some(PickAction::Attach(running[i].name.clone())));
        }
    } else if let Some(&j) = sess_idx.get(st.sel[1]) {
        return Flow::Return(Some(PickAction::Resume {
            session: sessions[j].clone(),
            current_dir,
        }));
    }
    Flow::Continue
}

/// Handle a menu item selection. Session items act; others set a hint.
fn menu_select(st: &mut State, menu: usize, item: usize) -> Flow {
    st.menu_open = None;
    if menu == 0 {
        match item {
            0 => return Flow::Return(Some(PickAction::New)),
            1 => st.tab = 1, // Resume → Sessions tab
            2 => st.tab = 0, // Attach → Running tab
            3 => st.filtering = true,
            4 => st.tab = 1,
            _ => {}
        }
        return Flow::Continue;
    }
    // Discoverability: surface the command to run from a shell.
    let label = MENUS[menu].1.get(item).copied().unwrap_or("");
    st.status = match label.split_once('—') {
        Some((_, cmd)) => format!("run from a shell: {}", cmd.trim()),
        None => format!("{label} — not yet wired into the control room"),
    };
    Flow::Continue
}

fn on_mouse(
    st: &mut State,
    m: event::MouseEvent,
    bar_r: Rect,
    tabs_r: Rect,
    table_r: Rect,
    last: usize,
) -> Option<Flow> {
    let (col, row) = (m.column, m.row);
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
                    // Click selects; the keyboard ⏎ (or double-click) acts.
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
