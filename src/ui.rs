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
pub fn sanitize_line(s: &str) -> String {
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
    /// BuildKit is exporting the completed build into an image.
    Finalizing,
    /// Build completed successfully
    Done,
    /// Build failed
    Error(String),
}

/// Returns the path to the build log file
pub fn build_log_path() -> PathBuf {
    let dir = dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("nemesis8");
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
    finalizing: bool,
    error: Option<String>,
    start: Instant,
    last_output: Instant,
    tick: u64,
    log_file: Option<std::fs::File>,
}

impl BuildState {
    fn new() -> Self {
        let log_file = std::fs::File::create(build_log_path()).ok();
        let start = Instant::now();
        Self {
            step: 0,
            total: 1,
            step_message: "Preparing build context...".into(),
            logs: Vec::new(),
            done: false,
            finalizing: false,
            error: None,
            start,
            last_output: start,
            tick: 0,
            log_file,
        }
    }

    fn write_log(&mut self, line: &str) {
        if let Some(ref mut f) = self.log_file {
            let _ = writeln!(f, "{line}");
        }
    }

    /// Retain the latest output lines verbatim for the scrolling log view.
    fn push_log(&mut self, line: String) {
        self.last_output = Instant::now();
        self.logs.push(line);
        if self.logs.len() > 500 {
            self.logs.drain(..self.logs.len() - 500);
        }
    }

    fn ratio(&self) -> f64 {
        if self.done && self.error.is_none() {
            return 1.0;
        }
        if self.finalizing {
            return 0.99;
        }
        if self.total == 0 {
            return 0.0;
        }
        // Cap below full until the build actually finishes — a long step (e.g.
        // the multi-crate cargo RUN) must never read as 100% while it's running.
        (self.step as f64 / self.total as f64).min(0.99)
    }

    fn spinner(&self) -> char {
        SPINNER[(self.tick as usize) % SPINNER.len()]
    }

    fn running_hint(&self) -> String {
        let secs = self.last_output.elapsed().as_secs();
        let age = if secs < 60 {
            format!("{secs}s")
        } else {
            format!("{}m {:02}s", secs / 60, secs % 60)
        };
        format!("Still building; last output {age} ago. Installing tools can take several minutes.")
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

/// Restores the terminal (cooked mode + main screen) on drop, so a panic, a
/// cancelled future, or an early error inside the build TUI can't leave the
/// shell in raw mode / the alternate screen — the "half in, half out" state
/// that survives `n8 build` into the parent shell. Mirrors `docker::TermGuard`
/// for the attach path; restoring twice is harmless (idempotent).
struct BuildTermGuard;

impl Drop for BuildTermGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }
}

/// Run the build progress TUI. Blocks until all events are consumed.
pub async fn run_build_progress(
    mut rx: mpsc::UnboundedReceiver<BuildEvent>,
) -> anyhow::Result<()> {
    enable_raw_mode()?;
    // From here on, ANY exit path — clean return, `?` error, panic unwind, or a
    // dropped/cancelled future — restores the terminal via this guard's Drop.
    let _guard = BuildTermGuard;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut state = BuildState::new();

    build_loop(&mut terminal, &mut state, &mut rx).await
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
                        state.last_output = Instant::now();
                    }
                    BuildEvent::Log(line) => {
                        state.write_log(&line);
                        let clean = sanitize_line(&line);
                        if !clean.trim().is_empty() {
                            state.push_log(clean);
                        }
                    }
                    BuildEvent::Finalizing => {
                        state.finalizing = true;
                        state.step_message = "Exporting image layers...".into();
                        state.last_output = Instant::now();
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
                        state.error = Some("Build output ended before completion was confirmed".into());
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

    // Coal background (#0f1117)
    let coal = Color::Rgb(15, 17, 23);
    let buf = frame.buffer_mut();
    for cell in buf.content.iter_mut() {
        cell.set_bg(coal);
    }

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
            " nemesis8 ",
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
            if state.finalizing {
                "FAILED while finalizing image".into()
            } else {
                format!("FAILED at step {}/{}", state.step, state.total)
            }
        } else {
            "Build complete".into()
        }
    } else if state.finalizing {
        "Finalizing image".into()
    } else {
        format!(
            "Step {}/{} \u{2502} ~{pct}%",
            state.step, state.total
        )
    };

    let gauge_style = if state.error.is_some() {
        Style::default().fg(Color::Red).bg(Color::DarkGray)
    } else if state.done {
        Style::default().fg(Color::Green).bg(Color::DarkGray)
    } else {
        Style::default().fg(Color::Rgb(0, 212, 170)).bg(Color::DarkGray)
    };

    let gauge = Gauge::default()
        .block(Block::default().borders(Borders::ALL).title(" Approximate progress (build steps) "))
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
    let mut step_lines = vec![step_line];
    if !state.done {
        step_lines.push(Line::from(Span::styled(
            state.running_hint(),
            Style::default().fg(Color::DarkGray),
        )));
    }
    frame.render_widget(Paragraph::new(step_lines), chunks[3]);

    // ── log area ──
    let log_width = chunks[4].width.saturating_sub(2) as usize; // inside borders
    let log_height = chunks[4].height.saturating_sub(2) as usize;
    let skip = state.logs.len().saturating_sub(log_height);
    let visible: Vec<Line> = state.logs[skip..]
        .iter()
        .map(|l| {
            let truncated: String = l.chars().take(log_width).collect();
            let style = if l.starts_with("Step ") {
                Style::default().fg(Color::Rgb(0, 212, 170))
            } else if l.contains("error") || l.contains("Error") {
                Style::default().fg(Color::Red)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            Line::from(Span::styled(truncated, style))
        })
        .collect();

    let log_block = Paragraph::new(visible)
        .block(Block::default().borders(Borders::ALL).title(" Latest build output "));
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
                Style::default().fg(Color::Rgb(0, 212, 170)),
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

/// Recognize BuildKit's exporter phase, excluding RUN output and completion lines.
pub fn is_build_finalizing(line: &str) -> bool {
    let Some(rest) = line.trim().strip_prefix('#') else {
        return false;
    };
    let Some((node, description)) = rest.split_once(' ') else {
        return false;
    };
    node.parse::<u32>().is_ok() && description.trim() == "exporting to image"
}

/// Parse a build-step line into (current, total, description). Understands all
/// three runtimes' formats:
///   legacy docker:  "Step 3/12 : RUN apt-get update"
///   podman:         "STEP 3/12: FROM docker.io/..."
///   BuildKit plain: "#8 [3/12] RUN apt-get update"  /  "#8 [builder 3/12] RUN cargo build"
/// Returns None for non-step lines (logs, "#8 DONE", "[internal] load ...").
pub fn parse_docker_step(line: &str) -> Option<(u32, u32, String)> {
    let line = line.trim();

    // Legacy docker / podman: "Step|STEP <i>/<j>[ :|:] desc". Podman multi-stage
    // prefixes the stage — "[1/2] STEP 3/8: ..." — so strip a leading "[...]"
    // bracket first so the STEP parse still matches (BuildKit "#N" lines start
    // with '#', not '[', so they fall through to the branch below untouched).
    let core = match line.strip_prefix('[') {
        Some(after) => after
            .find(']')
            .map(|i| after[i + 1..].trim_start())
            .unwrap_or(line),
        None => line,
    };
    if let Some(rest) = core.strip_prefix("Step ").or_else(|| core.strip_prefix("STEP ")) {
        let slash = rest.find('/')?;
        let current: u32 = rest[..slash].parse().ok()?;
        let after_slash = &rest[slash + 1..];
        let end = after_slash.find(|c: char| !c.is_ascii_digit())?;
        let total: u32 = after_slash[..end].parse().ok()?;
        let desc = after_slash[end..].trim_start_matches([':', ' ']).trim().to_string();
        return Some((current, total, desc));
    }

    // BuildKit plain: "#<n> [<stage?> <i>/<j>] desc". The bracket may carry a
    // stage name ("[builder 3/8]") or padding ("[ 3/12]"); brackets without a
    // fraction ("[internal]", "[auth]") are not steps.
    if let Some(rest) = line.strip_prefix('#') {
        let sp = rest.find(' ')?;
        rest[..sp].parse::<u32>().ok()?;
        let inner = rest[sp..].trim_start().strip_prefix('[')?;
        let close = inner.find(']')?;
        let frac = inner[..close].rsplit(' ').next()?;
        let slash = frac.find('/')?;
        let current: u32 = frac[..slash].trim().parse().ok()?;
        let total: u32 = frac[slash + 1..].trim().parse().ok()?;
        let desc = inner[close + 1..].trim().to_string();
        if desc.is_empty() {
            return None;
        }
        return Some((current, total, desc));
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_displays_patience_hint_only_while_running() {
        let mut state = BuildState {
            step: 1,
            total: 2,
            step_message: "Installing provider tools".into(),
            logs: Vec::new(),
            done: false,
            finalizing: false,
            error: None,
            start: Instant::now(),
            last_output: Instant::now(),
            tick: 1,
            log_file: None,
        };
        for n in 0..30 {
            state.push_log(format!("Installing package {n:02}"));
        }
        let backend = ratatui::backend::TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let screen = |terminal: &Terminal<ratatui::backend::TestBackend>| {
            terminal.backend().buffer().content.iter()
                .map(|cell| cell.symbol())
                .collect::<String>()
        };
        terminal.draw(|frame| draw(frame, &state)).unwrap();
        let running = screen(&terminal);
        assert!(running.contains("Installing provider tools"));
        assert!(running.contains("Still building; last output 0s ago."));
        assert!(running.contains("Installing tools can take several minutes."));
        assert!(running.contains("Building..."));
        assert!(running.contains("Approximate progress (build steps)"));
        assert!(running.contains("~50%"));
        assert!(running.contains("Latest build output"));
        assert!(running.contains("Installing package 28"));
        assert!(running.contains("Installing package 29"));
        assert!(!running.contains("Installing package 00"));
        state.last_output = Instant::now() - std::time::Duration::from_secs(241);
        terminal.draw(|frame| draw(frame, &state)).unwrap();
        let quiet = screen(&terminal);
        assert!(quiet.contains("Still building; last output 4m 01s ago."));
        assert!(quiet.contains("~50%"));
        state.push_log("Installing package 30".into());
        terminal.draw(|frame| draw(frame, &state)).unwrap();
        let resumed = screen(&terminal);
        assert!(resumed.contains("Still building; last output 0s ago."));
        assert!(resumed.contains("Installing package 30"));
        state.finalizing = true;
        state.step_message = "Exporting image layers...".into();
        terminal.draw(|frame| draw(frame, &state)).unwrap();
        let finalizing = screen(&terminal);
        assert!(finalizing.contains("Finalizing image"));
        assert!(finalizing.contains("Exporting image layers..."));
        assert!(!finalizing.contains("~50%"));
        assert!(state.ratio() < 1.0);
        state.done = true;
        terminal.draw(|frame| draw(frame, &state)).unwrap();
        let complete = screen(&terminal);
        assert!(complete.contains("BUILD COMPLETE"));
        assert!(!complete.contains("Still building"));
        state.error = Some("Installation failed".into());
        terminal.draw(|frame| draw(frame, &state)).unwrap();
        let failed = screen(&terminal);
        assert!(failed.contains("BUILD FAILED"));
        assert!(failed.contains("Installation failed"));
        assert!(!failed.contains("BUILD COMPLETE"));
        assert!(!failed.contains("Still building"));
        assert!(state.ratio() < 1.0);
    }

    #[test]
    fn recognizes_exporter_without_mistaking_log_output_for_finalization() {
        assert!(is_build_finalizing("#39 exporting to image"));
        assert!(is_build_finalizing("  #7 exporting to image  "));
        assert!(!is_build_finalizing("#39 12.0 exporting to image"));
        assert!(!is_build_finalizing("#39 exporting layers"));
        assert!(!is_build_finalizing("#39 DONE 1.2s"));
        assert!(!is_build_finalizing("exporting to image"));
    }

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
    fn test_parse_step_podman() {
        let (c, t, d) = parse_docker_step("STEP 3/12: FROM docker.io/deepbluedynamics/nemesis8-base:latest").unwrap();
        assert_eq!((c, t), (3, 12));
        assert_eq!(d, "FROM docker.io/deepbluedynamics/nemesis8-base:latest");
    }

    #[test]
    fn test_parse_step_podman_multistage_prefix() {
        // Podman/buildah multi-stage prefixes the stage: "[1/2] STEP 3/8: ...".
        let (c, t, d) = parse_docker_step("[2/2] STEP 5/40: RUN python3 /tmp/install-providers.py").unwrap();
        assert_eq!((c, t), (5, 40));
        assert_eq!(d, "RUN python3 /tmp/install-providers.py");
        // And it must NOT swallow BuildKit "#N [i/j]" lines.
        let (c, t, _) = parse_docker_step("#8 [3/12] RUN apt-get update").unwrap();
        assert_eq!((c, t), (3, 12));
    }

    #[test]
    fn test_parse_step_buildkit_plain() {
        let (c, t, d) = parse_docker_step("#8 [3/12] RUN apt-get update").unwrap();
        assert_eq!((c, t), (3, 12));
        assert_eq!(d, "RUN apt-get update");
        // space-padded counter
        let (c, t, _) = parse_docker_step("#9 [ 4/12] COPY providers/ /opt/defaults/providers/").unwrap();
        assert_eq!((c, t), (4, 12));
        // named multi-stage
        let (c, t, d) = parse_docker_step("#14 [builder 3/8] RUN cargo build --release").unwrap();
        assert_eq!((c, t), (3, 8));
        assert_eq!(d, "RUN cargo build --release");
    }

    #[test]
    fn test_parse_step_buildkit_non_steps() {
        assert!(parse_docker_step("#8 DONE 1.2s").is_none());
        assert!(parse_docker_step("#1 [internal] load build definition from Dockerfile").is_none());
        assert!(parse_docker_step("#3 [auth] library/node:pull token for registry-1.docker.io").is_none());
        assert!(parse_docker_step("#8 0.450 Reading package lists...").is_none());
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
        // At step==total but NOT done, the bar caps below 100% (a long final
        // step must never read as complete while it's still running).
        s.step = 10;
        assert!((s.ratio() - 0.99).abs() < f64::EPSILON);
        // Only `done` reads as 100%.
        s.done = true;
        assert!((s.ratio() - 1.0).abs() < f64::EPSILON);
        s.error = Some("Build failed".into());
        assert!((s.ratio() - 0.99).abs() < f64::EPSILON);
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
    fn test_push_log_keeps_recent_lines_without_collapsing() {
        let mut state = BuildState::new();
        for n in 0..510 {
            state.push_log(format!("Compiling crate {n}"));
        }
        assert_eq!(state.logs.len(), 500);
        assert_eq!(state.logs.first().unwrap(), "Compiling crate 10");
        assert_eq!(state.logs.last().unwrap(), "Compiling crate 509");
    }

    #[test]
    fn test_push_log_different_prefix_no_collapse() {
        let mut state = BuildState::new();
        state.push_log("   Compiling foo v1.0".into());
        state.push_log("Step 3/10 : RUN something".into());
        assert_eq!(state.logs.len(), 2);
    }
}
