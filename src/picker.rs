//! `n8 resume` picker — interactive session selector.
//!
//! When the user runs `n8 resume` without an id, this opens a ratatui list,
//! shows every session annotated with its provider, and returns the one they
//! select. The resume flow then uses that session's id; the existing
//! provider auto-detection (matching path against the registry's
//! session_dirs) handles the rest, so picking an antigravity session works
//! without `--provider antigravity`.

use anyhow::Result;
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
        KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{
        disable_raw_mode, enable_raw_mode, supports_keyboard_enhancement, EnterAlternateScreen,
        LeaveAlternateScreen,
    },
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Layout, Margin},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
    Terminal,
};
use std::io;

use crate::session::SessionInfo;

/// Open a fullscreen list of sessions; return the selected one or None on
/// cancel. Sessions should already be sorted newest-first.
pub fn pick_session(sessions: Vec<SessionInfo>) -> Result<Option<SessionInfo>> {
    if sessions.is_empty() {
        return Ok(None);
    }

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut selected: usize = 0;
    // Live filter state. `/` enters filter mode; typing narrows the list by a
    // case-insensitive substring match on id/provider/workspace/modified. This
    // is an in-memory navigation aid — full transcript content search lives in
    // `n8 sessions <query>`, which doesn't have to stay responsive per-keystroke.
    let mut query = String::new();
    let mut filtering = false;

    let result: Result<Option<SessionInfo>> = (|| {
        loop {
            // Indices into `sessions` that match the current filter.
            let visible: Vec<usize> = if query.is_empty() {
                (0..sessions.len()).collect()
            } else {
                let q = query.to_lowercase();
                sessions
                    .iter()
                    .enumerate()
                    .filter(|(_, s)| row_matches(s, &q))
                    .map(|(i, _)| i)
                    .collect()
            };
            let last = visible.len().saturating_sub(1);
            if selected > last {
                selected = last;
            }

            terminal.draw(|f| {
                let area = f.area();
                let chunks = Layout::vertical([
                    Constraint::Min(1),
                    Constraint::Length(1),
                ])
                .split(area);

                let items: Vec<ListItem> = visible
                    .iter()
                    .map(|&i| ListItem::new(format_row(&sessions[i])))
                    .collect();

                let title = if query.is_empty() {
                    format!("  resume: pick a session ({} total)  ", sessions.len())
                } else {
                    format!(
                        "  resume: /{}  ({}/{})  ",
                        query,
                        visible.len(),
                        sessions.len()
                    )
                };

                let list = List::new(items)
                    .block(Block::default().title(title).borders(Borders::ALL))
                    .highlight_style(
                        Style::default()
                            .bg(Color::Indexed(238))
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD),
                    )
                    .highlight_symbol("▶ ");

                let mut state = ListState::default();
                if !visible.is_empty() {
                    state.select(Some(selected));
                }
                f.render_stateful_widget(list, chunks[0], &mut state);

                let help = if filtering {
                    Line::from(vec![
                        Span::styled("filter: ", Style::default().fg(Color::Yellow)),
                        Span::styled(
                            format!("{query}▏"),
                            Style::default().fg(Color::White),
                        ),
                        Span::raw("   "),
                        Span::styled("⏎", Style::default().fg(Color::Yellow)),
                        Span::raw(" resume   "),
                        Span::styled("esc", Style::default().fg(Color::Yellow)),
                        Span::raw(" clear filter"),
                    ])
                } else {
                    Line::from(vec![
                        Span::styled("↑↓/jk", Style::default().fg(Color::Yellow)),
                        Span::raw(" select   "),
                        Span::styled("/", Style::default().fg(Color::Yellow)),
                        Span::raw(" filter   "),
                        Span::styled("⏎", Style::default().fg(Color::Yellow)),
                        Span::raw(" resume   "),
                        Span::styled("g/G", Style::default().fg(Color::Yellow)),
                        Span::raw(" top/bottom   "),
                        Span::styled("q/esc", Style::default().fg(Color::Yellow)),
                        Span::raw(" cancel"),
                    ])
                };
                f.render_widget(
                    Paragraph::new(help).style(Style::default().fg(Color::Gray)),
                    chunks[1].inner(Margin::new(1, 0)),
                );
            })?;

            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }

                // Filter mode: printable keys edit the query; navigation +
                // selection still work via the non-character keys below.
                if filtering {
                    match key.code {
                        KeyCode::Esc => {
                            filtering = false;
                            query.clear();
                            selected = 0;
                            continue;
                        }
                        KeyCode::Backspace => {
                            query.pop();
                            selected = 0;
                            continue;
                        }
                        KeyCode::Char(c) => {
                            query.push(c);
                            selected = 0;
                            continue;
                        }
                        _ => {}
                    }
                }

                match key.code {
                    KeyCode::Char('/') if !filtering => filtering = true,
                    KeyCode::Char('q') | KeyCode::Esc => return Ok(None),
                    KeyCode::Up | KeyCode::Char('k') => {
                        selected = selected.saturating_sub(1);
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        if selected < last {
                            selected += 1;
                        }
                    }
                    KeyCode::PageUp => selected = selected.saturating_sub(10),
                    KeyCode::PageDown => selected = (selected + 10).min(last),
                    KeyCode::Home | KeyCode::Char('g') => selected = 0,
                    KeyCode::End | KeyCode::Char('G') => selected = last,
                    KeyCode::Enter => {
                        if let Some(&i) = visible.get(selected) {
                            return Ok(Some(sessions[i].clone()));
                        }
                    }
                    _ => {}
                }
            }
        }
    })();

    // Always tear down the alt screen even on error.
    disable_raw_mode().ok();
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )
    .ok();
    terminal.show_cursor().ok();

    result
}

/// A running agent container — an attach target in the unified picker.
#[derive(Clone)]
pub struct RunningAgent {
    pub name: String,
    pub provider: String,
    /// Displayable status (theme taxonomy: working / needs-input / …).
    pub state: crate::theme::AgentUiState,
    /// Human-friendly status, e.g. "Up 12 minutes".
    pub uptime: String,
    /// Last line of the container's log (best-effort, may be empty).
    pub last_log: String,
    /// The session id the container is writing, if resolvable (None → "—").
    pub session_id: Option<String>,
    /// Host workspace path, read from the container's /workspace bind mount.
    pub workspace: Option<String>,
}

/// What the unified resume/attach picker resolved to.
pub enum PickAction {
    /// Attach to a running container by name. (A live process keeps its own
    /// working directory, so there's no dir choice here.)
    Attach(String),
    /// Resume a past session. `current_dir` is true when the user chose to
    /// resume in the directory n8 was launched from (Ctrl+Enter / `.`) rather
    /// than the session's original workspace (plain Enter).
    Resume { session: SessionInfo, current_dir: bool },
    /// Start a brand-new session (home screen's "+ New session" entry).
    New,
}

// One rendered row: a section header (not selectable) or an item indexing
// into the running/sessions slices.
enum Row {
    Header(&'static str),
    New,
    Running(usize),
    Session(usize),
}

/// Unified "resume or attach" picker. Running containers (attach targets) and
/// past sessions (resume targets) appear as two sections in one list; Enter
/// does the right thing for the highlighted row — attach if it's live, resume
/// if it's a past session. `/` filters both sections. Both `n8 resume` and
/// `n8 attach` (no arg) open this.
pub fn pick_agent(
    running: Vec<RunningAgent>,
    sessions: Vec<SessionInfo>,
    show_new: bool,
) -> Result<Option<PickAction>> {
    // The home screen (show_new) always has at least the "+ New session" row,
    // so only bail-empty when this is a pure resume/attach picker.
    if !show_new && running.is_empty() && sessions.is_empty() {
        return Ok(None);
    }

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    // Enable the kitty keyboard protocol if the terminal supports it, so we can
    // tell Ctrl+Enter apart from Enter (legacy terminals send the same byte for
    // both). Where unsupported, `.` is the fallback for "resume in current dir".
    let kitty = matches!(supports_keyboard_enhancement(), Ok(true));
    if kitty {
        let _ = execute!(
            stdout,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        );
    }
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut selected: usize = 0; // index into the selectable (non-header) rows
    let mut query = String::new();
    let mut filtering = false;

    let result: Result<Option<PickAction>> = (|| {
        loop {
            let q = query.to_lowercase();
            let run_idx: Vec<usize> = running
                .iter()
                .enumerate()
                .filter(|(_, r)| query.is_empty() || run_matches(r, &q))
                .map(|(i, _)| i)
                .collect();
            let sess_idx: Vec<usize> = sessions
                .iter()
                .enumerate()
                .filter(|(_, s)| query.is_empty() || row_matches(s, &q))
                .map(|(i, _)| i)
                .collect();

            // Build rows with a header per non-empty section.
            let mut rows: Vec<Row> = Vec::new();
            // "+ New session" only on the home screen, and only when not
            // filtering (the filter narrows existing agents, not this action).
            if show_new && query.is_empty() {
                rows.push(Row::Header("NEW"));
                rows.push(Row::New);
            }
            if !run_idx.is_empty() {
                rows.push(Row::Header("RUNNING  (⏎ attach)"));
                rows.extend(run_idx.iter().map(|&i| Row::Running(i)));
            }
            if !sess_idx.is_empty() {
                rows.push(Row::Header("SESSIONS  (⏎ resume)"));
                rows.extend(sess_idx.iter().map(|&j| Row::Session(j)));
            }
            let selectable: Vec<usize> = rows
                .iter()
                .enumerate()
                .filter(|(_, r)| !matches!(r, Row::Header(_)))
                .map(|(i, _)| i)
                .collect();
            let last = selectable.len().saturating_sub(1);
            if selected > last {
                selected = last;
            }

            terminal.draw(|f| {
                let area = f.area();
                let chunks =
                    Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(area);

                let items: Vec<ListItem> = rows
                    .iter()
                    .map(|r| match r {
                        Row::Header(h) => ListItem::new(Line::from(Span::styled(
                            *h,
                            Style::default()
                                .fg(Color::Indexed(244))
                                .add_modifier(Modifier::BOLD),
                        ))),
                        Row::New => ListItem::new(Line::from(Span::styled(
                            "+ New session",
                            Style::default()
                                .fg(Color::Green)
                                .add_modifier(Modifier::BOLD),
                        ))),
                        Row::Running(i) => ListItem::new(format_running(&running[*i])),
                        Row::Session(j) => ListItem::new(format_row(&sessions[*j])),
                    })
                    .collect();

                let total = running.len() + sessions.len();
                let title = if query.is_empty() {
                    format!("  n8 — resume or attach ({total} agents)  ")
                } else {
                    format!("  n8 — /{}  ({} shown)  ", query, selectable.len())
                };

                let list = List::new(items)
                    .block(Block::default().title(title).borders(Borders::ALL))
                    .highlight_style(
                        Style::default()
                            .bg(Color::Indexed(238))
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD),
                    )
                    .highlight_symbol("▶ ");

                let mut state = ListState::default();
                if let Some(&rowpos) = selectable.get(selected) {
                    state.select(Some(rowpos));
                }
                f.render_stateful_widget(list, chunks[0], &mut state);

                let help = if filtering {
                    Line::from(vec![
                        Span::styled("filter: ", Style::default().fg(Color::Yellow)),
                        Span::styled(format!("{query}▏"), Style::default().fg(Color::White)),
                        Span::raw("   "),
                        Span::styled("⏎", Style::default().fg(Color::Yellow)),
                        Span::raw(" go   "),
                        Span::styled("esc", Style::default().fg(Color::Yellow)),
                        Span::raw(" clear"),
                    ])
                } else {
                    Line::from(vec![
                        Span::styled("↑↓/jk", Style::default().fg(Color::Yellow)),
                        Span::raw(" move   "),
                        Span::styled("/", Style::default().fg(Color::Yellow)),
                        Span::raw(" filter   "),
                        Span::styled("⏎", Style::default().fg(Color::Yellow)),
                        Span::raw(" attach / resume in its dir   "),
                        Span::styled("^⏎ / .", Style::default().fg(Color::Yellow)),
                        Span::raw(" resume here   "),
                        Span::styled("q", Style::default().fg(Color::Yellow)),
                        Span::raw(" cancel"),
                    ])
                };
                f.render_widget(
                    Paragraph::new(help).style(Style::default().fg(Color::Gray)),
                    chunks[1].inner(Margin::new(1, 0)),
                );
            })?;

            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                if filtering {
                    match key.code {
                        KeyCode::Esc => {
                            filtering = false;
                            query.clear();
                            selected = 0;
                            continue;
                        }
                        KeyCode::Backspace => {
                            query.pop();
                            selected = 0;
                            continue;
                        }
                        KeyCode::Char(c) => {
                            query.push(c);
                            selected = 0;
                            continue;
                        }
                        _ => {}
                    }
                }
                match key.code {
                    KeyCode::Char('/') if !filtering => filtering = true,
                    KeyCode::Char('q') | KeyCode::Esc => return Ok(None),
                    KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
                    KeyCode::Down | KeyCode::Char('j') => {
                        if selected < last {
                            selected += 1;
                        }
                    }
                    KeyCode::PageUp => selected = selected.saturating_sub(10),
                    KeyCode::PageDown => selected = (selected + 10).min(last),
                    KeyCode::Home | KeyCode::Char('g') => selected = 0,
                    KeyCode::End | KeyCode::Char('G') => selected = last,
                    // `.` (universal) or Ctrl+Enter (kitty-capable terminals):
                    // resume in the dir n8 was launched from, not the session's.
                    KeyCode::Char('.') if !filtering => {
                        if let Some(act) = resolve(&rows, &selectable, selected, &running, &sessions, true) {
                            return Ok(Some(act));
                        }
                    }
                    KeyCode::Enter => {
                        let current_dir = key.modifiers.contains(KeyModifiers::CONTROL);
                        if let Some(act) = resolve(&rows, &selectable, selected, &running, &sessions, current_dir) {
                            return Ok(Some(act));
                        }
                    }
                    _ => {}
                }
            }
        }
    })();

    if kitty {
        let _ = execute!(terminal.backend_mut(), PopKeyboardEnhancementFlags);
    }
    disable_raw_mode().ok();
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )
    .ok();
    terminal.show_cursor().ok();
    result
}

/// Resolve the highlighted row into a PickAction. `current_dir` only affects
/// session (resume) rows; attach rows ignore it (a live process keeps its dir).
fn resolve(
    rows: &[Row],
    selectable: &[usize],
    selected: usize,
    running: &[RunningAgent],
    sessions: &[SessionInfo],
    current_dir: bool,
) -> Option<PickAction> {
    let rowpos = *selectable.get(selected)?;
    match rows.get(rowpos)? {
        Row::New => Some(PickAction::New),
        Row::Running(i) => Some(PickAction::Attach(running[*i].name.clone())),
        Row::Session(j) => Some(PickAction::Resume {
            session: sessions[*j].clone(),
            current_dir,
        }),
        Row::Header(_) => None,
    }
}

fn run_matches(r: &RunningAgent, q: &str) -> bool {
    r.name.to_lowercase().contains(q)
        || r.provider.to_lowercase().contains(q)
        || r.last_log.to_lowercase().contains(q)
}

fn format_running(r: &RunningAgent) -> Line<'static> {
    let log: String = r.last_log.chars().take(60).collect();
    Line::from(vec![
        Span::styled(format!("{:<16}  ", r.name), Style::default().fg(Color::Cyan)),
        Span::styled(
            format!("{:<10}  ", r.provider),
            Style::default().fg(Color::Green),
        ),
        Span::styled(
            format!("{:<14}  ", r.uptime),
            Style::default().fg(Color::Gray),
        ),
        Span::styled(
            if log.is_empty() {
                String::new()
            } else {
                format!("› {log}")
            },
            Style::default().fg(Color::DarkGray),
        ),
    ])
}

/// Does this session match the (already-lowercased) filter query? Matches the
/// same fields the row displays: id, provider, workspace, modified timestamp.
fn row_matches(s: &SessionInfo, q: &str) -> bool {
    s.id.to_lowercase().contains(q)
        || s.provider.as_deref().unwrap_or("").to_lowercase().contains(q)
        || s.workspace.as_deref().unwrap_or("").to_lowercase().contains(q)
        || s.modified.as_deref().unwrap_or("").to_lowercase().contains(q)
}

fn format_row(s: &SessionInfo) -> Line<'static> {
    let id_short: String = s.id.chars().take(8).collect();
    let provider = s.provider.clone().unwrap_or_else(|| "-".into());
    let modified: String = s
        .modified
        .as_deref()
        .map(|m| m.chars().take(19).collect())
        .unwrap_or_else(|| "unknown".into());
    let size = format_size(s.size_bytes);
    let workspace = s.workspace.clone().unwrap_or_default();

    Line::from(vec![
        Span::styled(
            format!("{id_short:<8}  "),
            Style::default().fg(Color::Cyan),
        ),
        Span::styled(
            format!("{provider:<12}  "),
            Style::default().fg(Color::Green),
        ),
        Span::raw(format!("{modified:<19}  ")),
        Span::styled(format!("{size:>9}  "), Style::default().fg(Color::Gray)),
        Span::styled(workspace, Style::default().fg(Color::DarkGray)),
    ])
}

fn format_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}
