//! `n8` new-session launcher — a small form to pick provider / model / danger
//! before starting a fresh interactive session. Reached from the home screen's
//! "+ New session" entry. Fields prefill from config + flags.

use anyhow::Result;
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Layout, Margin},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Terminal,
};
use std::io;

/// The choices made in the launcher.
pub struct NewSession {
    pub provider: String,
    pub model: Option<String>,
    pub danger: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum Field {
    Provider,
    Model,
    Danger,
}

/// Open the new-session form. `providers` is the installed provider list;
/// `init_*` prefill from config/flags. Returns the selection, or None on cancel.
pub fn new_session(
    providers: Vec<String>,
    init_provider: &str,
    init_model: Option<&str>,
    init_danger: bool,
) -> Result<Option<NewSession>> {
    let providers = if providers.is_empty() {
        vec!["codex".to_string(), "gemini".to_string(), "claude".to_string()]
    } else {
        providers
    };
    // Open on the configured provider; if it isn't in the list, fall back to
    // codex (the default) rather than whatever sorts first — so an unmatched
    // provider never silently lands you on a different one.
    let mut provider_idx = providers
        .iter()
        .position(|p| p == init_provider)
        .or_else(|| providers.iter().position(|p| p == "codex"))
        .unwrap_or(0);
    let mut model = init_model.unwrap_or("").to_string();
    let mut danger = init_danger;
    let mut field = Field::Provider;

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result: Result<Option<NewSession>> = (|| {
        loop {
            terminal.draw(|f| {
                let area = f.area();
                let chunks =
                    Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(area);

                let row = |label: &str, value: Line<'static>, focused: bool| -> Line<'static> {
                    let marker = if focused { "▶ " } else { "  " };
                    let mut spans = vec![Span::styled(
                        format!("{marker}{label:<9} "),
                        Style::default().fg(if focused { Color::Yellow } else { Color::Gray }),
                    )];
                    spans.extend(value.spans);
                    Line::from(spans)
                };

                let provider_val = Line::from(Span::styled(
                    format!("‹ {} ›", providers[provider_idx]),
                    Style::default().fg(Color::Green),
                ));
                let model_val = Line::from(Span::styled(
                    if model.is_empty() {
                        "(provider default)".to_string()
                    } else {
                        format!("{model}▏")
                    },
                    Style::default().fg(Color::White),
                ));
                let danger_val = Line::from(Span::styled(
                    format!("[{}] skip approvals + sandbox", if danger { "x" } else { " " }),
                    Style::default().fg(if danger { Color::Red } else { Color::Gray }),
                ));

                let body = vec![
                    Line::from(""),
                    row("Provider", provider_val, field == Field::Provider),
                    row("Model", model_val, field == Field::Model),
                    row("Danger", danger_val, field == Field::Danger),
                ];

                f.render_widget(
                    Paragraph::new(body).block(
                        Block::default()
                            .title("  n8 — new session  ")
                            .borders(Borders::ALL),
                    ),
                    chunks[0],
                );

                let help = Line::from(vec![
                    Span::styled("↑↓/tab", Style::default().fg(Color::Yellow)),
                    Span::raw(" field   "),
                    Span::styled("←→", Style::default().fg(Color::Yellow)),
                    Span::raw(" change   "),
                    Span::styled("⏎", Style::default().fg(Color::Yellow)),
                    Span::raw(" launch   "),
                    Span::styled("esc", Style::default().fg(Color::Yellow)),
                    Span::raw(" cancel"),
                ]);
                f.render_widget(
                    Paragraph::new(help).style(Style::default().fg(Color::Gray)),
                    chunks[1].inner(Margin::new(1, 0)),
                );
            })?;

            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                match key.code {
                    KeyCode::Esc => return Ok(None),
                    KeyCode::Enter => {
                        return Ok(Some(NewSession {
                            provider: providers[provider_idx].clone(),
                            model: {
                                let m = model.trim();
                                if m.is_empty() {
                                    None
                                } else {
                                    Some(m.to_string())
                                }
                            },
                            danger,
                        }));
                    }
                    KeyCode::Tab | KeyCode::Down => {
                        field = match field {
                            Field::Provider => Field::Model,
                            Field::Model => Field::Danger,
                            Field::Danger => Field::Provider,
                        };
                    }
                    KeyCode::BackTab | KeyCode::Up => {
                        field = match field {
                            Field::Provider => Field::Danger,
                            Field::Model => Field::Provider,
                            Field::Danger => Field::Model,
                        };
                    }
                    KeyCode::Left => match field {
                        Field::Provider => {
                            provider_idx = if provider_idx == 0 {
                                providers.len() - 1
                            } else {
                                provider_idx - 1
                            };
                        }
                        Field::Danger => danger = !danger,
                        Field::Model => {}
                    },
                    KeyCode::Right => match field {
                        Field::Provider => provider_idx = (provider_idx + 1) % providers.len(),
                        Field::Danger => danger = !danger,
                        Field::Model => {}
                    },
                    KeyCode::Char(' ') if field == Field::Danger => danger = !danger,
                    KeyCode::Char(c) if field == Field::Model => model.push(c),
                    KeyCode::Backspace if field == Field::Model => {
                        model.pop();
                    }
                    _ => {}
                }
            }
        }
    })();

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
