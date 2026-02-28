use std::io::{self, IsTerminal};
use std::time::Instant;

use crossterm::{
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    prelude::*,
    widgets::{Block, Borders, Gauge, Paragraph, Wrap},
};
use tokio::sync::mpsc;

const SPINNER: &[char] = &['\u{28FB}', '\u{28FD}', '\u{28FE}', '\u{28F7}', '\u{28EF}', '\u{28DF}', '\u{28BF}', '\u{287F}'];

/// Events sent from the Docker build stream to the TUI
pub enum BuildEvent {
    /// A build step parsed from "Step X/Y : description"
    Step {
        current: u32,
        total: u32,
        message: String,
    },
    /// A raw log line from the build output
    Log(String),
    /// Build completed successfully
    Done,
    /// Build failed
    Error(String),
}

/// Internal render state
struct BuildState {
    step: u32,
    total: u32,
    step_message: String,
    logs: Vec<String>,
    done: bool,
    error: Option<String>,
    start: Instant,
    tick: u64,
}

impl BuildState {
    fn new() -> Self {
        Self {
            step: 0,
            total: 1,
            step_message: "Preparing build context...".into(),
            logs: Vec::new(),
            done: false,
            error: None,
            start: Instant::now(),
            tick: 0,
        }
    }

    fn ratio(&self) -> f64 {
        if self.total == 0 {
            return 0.0;
        }
        (self.step as f64 / self.total as f64).min(1.0)
    }

    fn spinner(&self) -> char {
        SPINNER[(self.tick as usize) % SPINNER.len()]
    }

    fn elapsed_str(&self) -> String {
        let secs = self.start.elapsed().as_secs();
        if secs < 60 {
            format!("{secs}s")
        } else {
            format!("{}m {:02}s", secs / 60, secs % 60)
        }
    }
}

/// Returns true if stdout is an interactive terminal
pub fn is_interactive() -> bool {
    io::stdout().is_terminal()
}

/// Run the build progress TUI. Blocks until all events are consumed.
pub async fn run_build_progress(
    mut rx: mpsc::UnboundedReceiver<BuildEvent>,
) -> anyhow::Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut state = BuildState::new();

    let result = build_loop(&mut terminal, &mut state, &mut rx).await;

    // Always restore terminal, even on error
    disable_raw_mode()?;
    execute!(io::stdout(), LeaveAlternateScreen)?;

    result
}

async fn build_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    state: &mut BuildState,
    rx: &mut mpsc::UnboundedReceiver<BuildEvent>,
) -> anyhow::Result<()> {
    loop {
        // Drain all available events
        loop {
            match rx.try_recv() {
                Ok(event) => match event {
                    BuildEvent::Step {
                        current,
                        total,
                        message,
                    } => {
                        state.step = current;
                        state.total = total;
                        state.step_message = message;
                    }
                    BuildEvent::Log(line) => {
                        state.logs.push(line);
                        if state.logs.len() > 500 {
                            state.logs.drain(..state.logs.len() - 500);
                        }
                    }
                    BuildEvent::Done => {
                        state.done = true;
                        state.step = state.total;
                    }
                    BuildEvent::Error(e) => {
                        state.error = Some(e);
                        state.done = true;
                    }
                },
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    if !state.done {
                        state.done = true;
                    }
                    break;
                }
            }
        }

        state.tick += 1;
        terminal.draw(|frame| draw(frame, state))?;

        if state.done {
            // Show final frame briefly
            tokio::time::sleep(std::time::Duration::from_millis(800)).await;
            break;
        }

        tokio::time::sleep(std::time::Duration::from_millis(60)).await;
    }

    Ok(())
}

fn draw(frame: &mut Frame, state: &BuildState) {
    let area = frame.area();

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([
            Constraint::Length(1), // title
            Constraint::Length(1), // spacer
            Constraint::Length(3), // progress gauge
            Constraint::Length(2), // step info
            Constraint::Min(4),   // log area
            Constraint::Length(1), // status bar
        ])
        .split(area);

    // ── title ──
    let title = Line::from(vec![
        Span::styled(
            " nemisis8 ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("build", Style::default().fg(Color::White)),
    ]);
    frame.render_widget(
        Paragraph::new(title).alignment(Alignment::Center),
        chunks[0],
    );

    // ── progress gauge ──
    let ratio = state.ratio();
    let pct = (ratio * 100.0) as u16;
    let label = if state.done {
        if state.error.is_some() {
            format!("FAILED at step {}/{}", state.step, state.total)
        } else {
            format!("{}/{} complete", state.total, state.total)
        }
    } else {
        format!(
            "Step {}/{} \u{2502} {pct}%",
            state.step, state.total
        )
    };

    let gauge_style = if state.error.is_some() {
        Style::default().fg(Color::Red).bg(Color::DarkGray)
    } else if state.done {
        Style::default().fg(Color::Green).bg(Color::DarkGray)
    } else {
        Style::default().fg(Color::Cyan).bg(Color::DarkGray)
    };

    let gauge = Gauge::default()
        .block(Block::default().borders(Borders::ALL).title(" Progress "))
        .gauge_style(gauge_style)
        .ratio(ratio)
        .label(label);
    frame.render_widget(gauge, chunks[2]);

    // ── current step ──
    let step_line = if state.done && state.error.is_none() {
        Line::from(Span::styled(
            "Build complete.",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ))
    } else if state.done && state.error.is_some() {
        Line::from(Span::styled(
            state.error.as_deref().unwrap_or("unknown error"),
            Style::default().fg(Color::Red),
        ))
    } else {
        Line::from(Span::styled(
            &state.step_message,
            Style::default().fg(Color::Yellow),
        ))
    };
    frame.render_widget(Paragraph::new(step_line).wrap(Wrap { trim: true }), chunks[3]);

    // ── log area ──
    let log_height = chunks[4].height.saturating_sub(2) as usize;
    let skip = state.logs.len().saturating_sub(log_height);
    let visible: Vec<Line> = state.logs[skip..]
        .iter()
        .map(|l| {
            let style = if l.starts_with("Step ") {
                Style::default().fg(Color::Cyan)
            } else if l.contains("error") || l.contains("Error") {
                Style::default().fg(Color::Red)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            Line::from(Span::styled(l.as_str(), style))
        })
        .collect();

    let log_block = Paragraph::new(visible)
        .block(Block::default().borders(Borders::ALL).title(" Build Log "))
        .wrap(Wrap { trim: false });
    frame.render_widget(log_block, chunks[4]);

    // ── status bar ──
    let elapsed = state.elapsed_str();
    let status = if state.done {
        if state.error.is_some() {
            Line::from(vec![
                Span::styled(
                    " \u{2718} BUILD FAILED ",
                    Style::default()
                        .fg(Color::Red)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!(" ({elapsed})"),
                    Style::default().fg(Color::DarkGray),
                ),
            ])
        } else {
            Line::from(vec![
                Span::styled(
                    " \u{2714} BUILD COMPLETE ",
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!(" ({elapsed})"),
                    Style::default().fg(Color::DarkGray),
                ),
            ])
        }
    } else {
        Line::from(vec![
            Span::styled(
                format!(" {} Building... ", state.spinner()),
                Style::default().fg(Color::Cyan),
            ),
            Span::styled(
                format!("({elapsed})"),
                Style::default().fg(Color::DarkGray),
            ),
        ])
    };
    frame.render_widget(
        Paragraph::new(status).alignment(Alignment::Center),
        chunks[5],
    );
}

/// Parse "Step X/Y : description" from Docker build output.
/// Returns (current, total, description) or None.
pub fn parse_docker_step(line: &str) -> Option<(u32, u32, String)> {
    let line = line.trim();
    if !line.starts_with("Step ") {
        return None;
    }
    let rest = &line[5..];
    let slash = rest.find('/')?;
    let current: u32 = rest[..slash].parse().ok()?;

    let after_slash = &rest[slash + 1..];
    let space = after_slash.find(' ')?;
    let total: u32 = after_slash[..space].parse().ok()?;

    let desc = if let Some(pos) = after_slash.find(" : ") {
        after_slash[pos + 3..].to_string()
    } else {
        after_slash[space..].trim().to_string()
    };

    Some((current, total, desc))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_step_basic() {
        let (c, t, d) = parse_docker_step("Step 3/33 : RUN apt-get update").unwrap();
        assert_eq!(c, 3);
        assert_eq!(t, 33);
        assert_eq!(d, "RUN apt-get update");
    }

    #[test]
    fn test_parse_step_first() {
        let (c, t, d) = parse_docker_step("Step 1/33 : FROM node:24-slim").unwrap();
        assert_eq!(c, 1);
        assert_eq!(t, 33);
        assert_eq!(d, "FROM node:24-slim");
    }

    #[test]
    fn test_parse_step_last() {
        let (c, t, d) = parse_docker_step("Step 33/33 : CMD [\"tini\", \"--\"]").unwrap();
        assert_eq!(c, 33);
        assert_eq!(t, 33);
        assert_eq!(d, "CMD [\"tini\", \"--\"]");
    }

    #[test]
    fn test_parse_step_not_a_step() {
        assert!(parse_docker_step(" ---> Using cache").is_none());
        assert!(parse_docker_step("").is_none());
        assert!(parse_docker_step("Reading package lists...").is_none());
        assert!(parse_docker_step("Get:1 http://deb.debian.org").is_none());
    }

    #[test]
    fn test_parse_step_with_whitespace() {
        let (c, t, d) = parse_docker_step("  Step 5/10 : COPY . .  ").unwrap();
        assert_eq!(c, 5);
        assert_eq!(t, 10);
        assert_eq!(d, "COPY . .");
    }

    #[test]
    fn test_build_state_ratio() {
        let mut s = BuildState::new();
        assert_eq!(s.ratio(), 0.0);
        s.step = 5;
        s.total = 10;
        assert!((s.ratio() - 0.5).abs() < f64::EPSILON);
        s.step = 10;
        assert!((s.ratio() - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_build_state_ratio_zero_total() {
        let mut s = BuildState::new();
        s.total = 0;
        assert_eq!(s.ratio(), 0.0);
    }

    #[test]
    fn test_elapsed_str_seconds() {
        let s = BuildState::new();
        let e = s.elapsed_str();
        assert!(e.ends_with('s'));
    }
}
