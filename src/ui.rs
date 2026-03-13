use std::io::{self, IsTerminal, Write as _};
use std::path::PathBuf;
use std::time::Instant;

use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    prelude::*,
    widgets::{Block, Borders, Gauge, Paragraph},
};
use tokio::sync::mpsc;

const SPINNER: &[char] = &['\u{28FB}', '\u{28FD}', '\u{28FE}', '\u{28F7}', '\u{28EF}', '\u{28DF}', '\u{28BF}', '\u{287F}'];

/// Strip ANSI escape sequences and control characters from a string.
/// Also handles \r-delimited partial lines by keeping only the last segment.
fn sanitize_line(s: &str) -> String {
    // Handle carriage-return overwrites: keep only the last \r segment
    let s = if let Some(pos) = s.rfind('\r') {
        &s[pos + 1..]
    } else {
        s
    };

    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // Skip ESC [ ... final_byte sequences
            if chars.peek() == Some(&'[') {
                chars.next();
                while let Some(&next) = chars.peek() {
                    chars.next();
                    if next.is_ascii_alphabetic() || next == 'm' || next == 'K' || next == 'H' || next == 'J' {
                        break;
                    }
                }
            }
        } else if c == '\x08' {
            // Backspace: remove last char from output
            out.pop();
        } else if c.is_control() && c != '\t' {
            // Skip other control chars
        } else {
            out.push(c);
        }
    }
    out
}

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

/// Returns the path to the build log file
pub fn build_log_path() -> PathBuf {
    let dir = dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("nemisis8");
    std::fs::create_dir_all(&dir).ok();
    dir.join("build.log")
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
    log_file: Option<std::fs::File>,
}

impl BuildState {
    fn new() -> Self {
        let log_file = std::fs::File::create(build_log_path()).ok();
        Self {
            step: 0,
            total: 1,
            step_message: "Preparing build context...".into(),
            logs: Vec::new(),
            done: false,
            error: None,
            start: Instant::now(),
            tick: 0,
            log_file,
        }
    }

    fn write_log(&mut self, line: &str) {
        if let Some(ref mut f) = self.log_file {
            let _ = writeln!(f, "{line}");
        }
    }

    /// Add a log line, collapsing consecutive lines that share the same
    /// prefix (e.g. multiple "  Compiling foo" or "  Downloading bar" lines
    /// replace each other instead of stacking).
    fn push_log(&mut self, line: String) {
        let prefix = log_prefix(&line);

        // If the last line has the same prefix, replace it
        if let Some(last) = self.logs.last_mut() {
            if !prefix.is_empty() && log_prefix(last) == prefix {
                *last = line;
                return;
            }
        }

        self.logs.push(line);
        if self.logs.len() > 500 {
            self.logs.drain(..self.logs.len() - 500);
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

/// Extract the "action prefix" from a build log line for collapse grouping.
/// Lines like "  Compiling foo v1.2" → "Compiling"
/// Lines like "  Downloading bar" → "Downloading"
/// Lines like "Sending build context" → "Sending"
/// Lines like " ---> abc123" → "--->"
/// Progress bars (━━━) → "progress"
/// Everything else → "" (no collapsing)
fn log_prefix(line: &str) -> &'static str {
    let t = line.trim_start();
    if t.starts_with("Compiling ") { return "Compiling"; }
    if t.starts_with("Downloading ") { return "Downloading"; }
    if t.starts_with("Installing ") { return "Installing"; }
    if t.starts_with("Unpacking ") { return "Unpacking"; }
    if t.starts_with("Setting up ") { return "Setting up"; }
    if t.starts_with("Get:") { return "Get"; }
    if t.starts_with("Sending build context") { return "Sending"; }
    if t.starts_with("---> ") { return "--->"; }
    if t.contains('\u{2501}') || t.contains('\u{2503}') || t.contains('\u{2588}') {
        return "progress";
    }
    ""
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
                        ref message,
                    } => {
                        state.write_log(&format!("Step {current}/{total} : {message}"));
                        state.step = current;
                        state.total = total;
                        state.step_message = sanitize_line(message);
                    }
                    BuildEvent::Log(line) => {
                        state.write_log(&line);
                        let clean = sanitize_line(&line);
                        if !clean.trim().is_empty() {
                            state.push_log(clean);
                        }
                    }
                    BuildEvent::Done => {
                        state.write_log("BUILD COMPLETE");
                        state.done = true;
                        state.step = state.total;
                    }
                    BuildEvent::Error(e) => {
                        state.write_log(&format!("BUILD ERROR: {e}"));
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

        // Check for Ctrl+C / q to abort
        if event::poll(std::time::Duration::from_millis(0))? {
            if let Event::Key(key) = event::read()? {
                if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                    state.error = Some("Cancelled by user".into());
                    state.done = true;
                }
                if key.code == KeyCode::Char('q') {
                    state.error = Some("Cancelled by user".into());
                    state.done = true;
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

    // Clear the entire frame first
    frame.render_widget(
        Paragraph::new("").style(Style::default().bg(Color::Black)),
        area,
    );

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
    let step_text = if state.done && state.error.is_none() {
        Span::styled(
            "Build complete.",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        )
    } else if state.done && state.error.is_some() {
        Span::styled(
            state.error.as_deref().unwrap_or("unknown error"),
            Style::default().fg(Color::Red),
        )
    } else {
        Span::styled(
            &state.step_message,
            Style::default().fg(Color::Yellow),
        )
    };
    // Truncate step text to available width
    let step_width = chunks[3].width as usize;
    let step_str: String = step_text.content.chars().take(step_width).collect();
    let step_line = Line::from(Span::styled(step_str, step_text.style));
    frame.render_widget(Paragraph::new(step_line), chunks[3]);

    // ── log area ──
    let log_width = chunks[4].width.saturating_sub(2) as usize; // inside borders
    let log_height = chunks[4].height.saturating_sub(2) as usize;
    let skip = state.logs.len().saturating_sub(log_height);
    let visible: Vec<Line> = state.logs[skip..]
        .iter()
        .map(|l| {
            let truncated: String = l.chars().take(log_width).collect();
            let style = if l.starts_with("Step ") {
                Style::default().fg(Color::Cyan)
            } else if l.contains("error") || l.contains("Error") {
                Style::default().fg(Color::Red)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            Line::from(Span::styled(truncated, style))
        })
        .collect();

    let log_block = Paragraph::new(visible)
        .block(Block::default().borders(Borders::ALL).title(" Build Log "));
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

    #[test]
    fn test_sanitize_line_ansi() {
        let input = "\x1b[32mHello\x1b[0m world";
        assert_eq!(sanitize_line(input), "Hello world");
    }

    #[test]
    fn test_sanitize_line_cr() {
        let input = "old stuff\rnew line";
        assert_eq!(sanitize_line(input), "new line");
    }

    #[test]
    fn test_log_prefix_compiling() {
        assert_eq!(log_prefix("   Compiling foo v1.0"), "Compiling");
        assert_eq!(log_prefix("   Downloading bar"), "Downloading");
        assert_eq!(log_prefix("some other line"), "");
    }

    #[test]
    fn test_push_log_collapses() {
        let mut state = BuildState::new();
        state.push_log("   Compiling foo v1.0".into());
        state.push_log("   Compiling bar v2.0".into());
        state.push_log("   Compiling baz v3.0".into());
        // All three "Compiling" lines should collapse to just the last one
        assert_eq!(state.logs.len(), 1);
        assert_eq!(state.logs[0], "   Compiling baz v3.0");
    }

    #[test]
    fn test_push_log_different_prefix_no_collapse() {
        let mut state = BuildState::new();
        state.push_log("   Compiling foo v1.0".into());
        state.push_log("Step 3/10 : RUN something".into());
        assert_eq!(state.logs.len(), 2);
    }
}
