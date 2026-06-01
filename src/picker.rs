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
    let last = sessions.len().saturating_sub(1);

    let result: Result<Option<SessionInfo>> = (|| {
        loop {
            terminal.draw(|f| {
                let area = f.area();
                let chunks = Layout::vertical([
                    Constraint::Min(1),
                    Constraint::Length(1),
                ])
                .split(area);

                let items: Vec<ListItem> = sessions
                    .iter()
                    .map(|s| ListItem::new(format_row(s)))
                    .collect();

                let list = List::new(items)
                    .block(
                        Block::default()
                            .title(format!(
                                "  resume: pick a session ({} total)  ",
                                sessions.len()
                            ))
                            .borders(Borders::ALL),
                    )
                    .highlight_style(
                        Style::default()
                            .bg(Color::Indexed(238))
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD),
                    )
                    .highlight_symbol("▶ ");

                let mut state = ListState::default();
                state.select(Some(selected));
                f.render_stateful_widget(list, chunks[0], &mut state);

                let help = Paragraph::new(Line::from(vec![
                    Span::styled("↑↓/jk", Style::default().fg(Color::Yellow)),
                    Span::raw(" select   "),
                    Span::styled("⏎", Style::default().fg(Color::Yellow)),
                    Span::raw(" resume   "),
                    Span::styled("g/G", Style::default().fg(Color::Yellow)),
                    Span::raw(" top/bottom   "),
                    Span::styled("q/esc", Style::default().fg(Color::Yellow)),
                    Span::raw(" cancel"),
                ]))
                .style(Style::default().fg(Color::Gray));
                f.render_widget(help, chunks[1].inner(Margin::new(1, 0)));
            })?;

            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => return Ok(None),
                    KeyCode::Up | KeyCode::Char('k') => {
                        selected = selected.saturating_sub(1);
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        if selected < last {
                            selected += 1;
                        }
                    }
                    KeyCode::PageUp => {
                        selected = selected.saturating_sub(10);
                    }
                    KeyCode::PageDown => {
                        selected = (selected + 10).min(last);
                    }
                    KeyCode::Home | KeyCode::Char('g') => selected = 0,
                    KeyCode::End | KeyCode::Char('G') => selected = last,
                    KeyCode::Enter => return Ok(Some(sessions[selected].clone())),
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
