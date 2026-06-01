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
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
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
