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
    layout::{Constraint, Layout, Margin, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph},
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
                        KeyCode::Enter => {
                            filtering = false;
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

/// Outcome of the compact resume overlay (bare `n8 resume`).
pub enum QuickResume {
    /// Launch this — attach (running) or resume (saved), decided per row and
    /// dispatched through the SAME PickAction plumbing as the full picker.
    Pick(PickAction),
    /// Open the full resume/attach picker (filter, new session, the works).
    More,
}

/// One row of the overlay: a live container or a saved session, merged by
/// recency (running = most recent by definition, they're writing NOW).
enum QuickRow {
    Run(RunningAgent),
    Sess(SessionInfo),
}

/// A tight, centered "last 10" overlay — one keystroke back into whatever you
/// were doing, regardless of state: running, suspended, or saved. ↑↓/jk move,
/// 1–9/0 jump, ⏎ or `a` launches (attach vs resume decided for you), m (or ⏎
/// on more…) opens the full picker, q/esc cancels. Deliberately minimal: no
/// filter, no mouse — that's what more… is for.
pub fn pick_resume_quick(
    running: &[RunningAgent],
    sessions: &[SessionInfo],
) -> Result<Option<QuickResume>> {
    if running.is_empty() && sessions.is_empty() {
        return Ok(Some(QuickResume::More)); // nothing to show tightly — go full
    }
    // Running first; then saved sessions minus the ones a running container is
    // actively writing (they'd be duplicates — attach beats resume for those).
    let live_ids: Vec<&str> = running.iter().filter_map(|r| r.session_id.as_deref()).collect();
    let mut rows: Vec<QuickRow> = running.iter().cloned().map(QuickRow::Run).collect();
    rows.extend(
        sessions
            .iter()
            .filter(|s| !live_ids.contains(&s.id.as_str()))
            .take(10usize.saturating_sub(rows.len().min(10)))
            .cloned()
            .map(QuickRow::Sess),
    );
    rows.truncate(10);
    let n = rows.len();

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let _guard = ScreenGuard;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut selected: usize = 0; // 0..n = sessions, n = the more… row
    let result: Result<Option<QuickResume>> = (|| {
        loop {
            terminal.draw(|f| {
                let area = f.area();
                let w = area.width.saturating_sub(4).min(72).max(40);
                let h = ((n as u16) + 4).min(area.height); // rows + more… + borders/title
                let rect = Rect {
                    x: area.x + (area.width.saturating_sub(w)) / 2,
                    y: area.y + (area.height.saturating_sub(h)) / 2,
                    width: w,
                    height: h,
                };
                f.render_widget(Clear, rect);

                let mut items: Vec<ListItem> = rows
                    .iter()
                    .enumerate()
                    .map(|(i, r)| ListItem::new(format_quick_row(i, r, w)))
                    .collect();
                items.push(ListItem::new(Line::from(vec![
                    Span::styled("m ", Style::default().fg(Color::Yellow)),
                    Span::styled(
                        "more…  full picker: filter, new session",
                        Style::default().fg(Color::DarkGray),
                    ),
                ])));

                let list = List::new(items)
                    .block(
                        Block::default()
                            .title("  get back in — ⏎/a or 1–9,0   q close  ")
                            .borders(Borders::ALL)
                            .border_style(Style::default().fg(Color::DarkGray)),
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
                f.render_stateful_widget(list, rect, &mut state);
            })?;

            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                match key.code {
                    KeyCode::Esc | KeyCode::Char('q') => return Ok(None),
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        return Ok(None)
                    }
                    KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
                    KeyCode::Down | KeyCode::Char('j') => selected = (selected + 1).min(n),
                    KeyCode::Char('m') => return Ok(Some(QuickResume::More)),
                    KeyCode::Char(c @ '0'..='9') => {
                        let idx = if c == '0' { 9 } else { (c as u8 - b'1') as usize };
                        if idx < n {
                            return Ok(Some(QuickResume::Pick(quick_action(&rows[idx]))));
                        }
                    }
                    KeyCode::Enter | KeyCode::Char('a') => {
                        return Ok(if selected == n {
                            Some(QuickResume::More)
                        } else {
                            Some(QuickResume::Pick(quick_action(&rows[selected])))
                        });
                    }
                    _ => {}
                }
            }
        }
    })();
    result
}

/// Map a row to its launch action — attach for live containers, resume (into
/// the original workspace, same as the full picker's plain ⏎) for saved ones.
fn quick_action(row: &QuickRow) -> PickAction {
    match row {
        QuickRow::Run(r) => PickAction::Attach(r.name.clone()),
        QuickRow::Sess(s) => PickAction::Resume { session: s.clone(), current_dir: false },
    }
}

/// One compact row: jump digit, then live (● name state) or saved
/// (short id, provider, age, workspace tail).
fn format_quick_row(i: usize, row: &QuickRow, width: u16) -> Line<'static> {
    let digit = if i == 9 { '0' } else { (b'1' + i as u8) as char };
    match row {
        QuickRow::Run(r) => {
            let uptime = r.uptime.trim_start_matches("Up ").to_string();
            let ws = r
                .workspace
                .as_deref()
                .map(|w| {
                    std::path::Path::new(w)
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| w.to_string())
                })
                .unwrap_or_default();
            Line::from(vec![
                Span::styled(format!("{digit} "), Style::default().fg(Color::Yellow)),
                Span::styled("● ", Style::default().fg(Color::Green)),
                Span::styled(format!("{:<18}", r.name), Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
                Span::styled(format!("{:<12}", r.provider), Style::default().fg(Color::Green)),
                Span::raw(format!("{ws}  ")),
                Span::styled(format!("up {uptime}"), Style::default().fg(Color::DarkGray)),
            ])
        }
        QuickRow::Sess(s) => {
            let short: String = s.id.chars().take(8).collect();
            let prov = s.provider.clone().unwrap_or_default();
            let age = humanize_age(s.modified.as_deref());
            // Whatever's left after the fixed columns goes to the workspace tail.
            let fixed = 2 + 2 + 9 + 12 + 9; // digit, dot-slot, short, prov, age
            let ws_room = (width as usize).saturating_sub(fixed + 3);
            let ws_full = s.workspace.clone().unwrap_or_default();
            let ws = std::path::Path::new(&ws_full)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or(ws_full);
            let ws: String = ws.chars().take(ws_room).collect();
            Line::from(vec![
                Span::styled(format!("{digit} "), Style::default().fg(Color::Yellow)),
                Span::raw("  "),
                Span::styled(format!("{short:<9}"), Style::default().fg(Color::Cyan)),
                Span::styled(format!("{prov:<12}"), Style::default().fg(Color::Green)),
                Span::styled(format!("{age:>7}  "), Style::default().fg(Color::DarkGray)),
                Span::raw(ws),
            ])
        }
    }
}

/// "3m" / "2h" / "5d" ago-style age from an RFC3339 timestamp.
fn humanize_age(modified: Option<&str>) -> String {
    let Some(ts) = modified else { return String::new() };
    let Ok(t) = chrono::DateTime::parse_from_rfc3339(ts) else { return String::new() };
    let secs = (chrono::Utc::now() - t.with_timezone(&chrono::Utc)).num_seconds().max(0);
    match secs {
        0..=59 => "now".to_string(),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86_399 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86_400),
    }
}

/// The optional image layers + agent CLIs chosen on the `n8 build` checkbox screen.
pub struct BuildOptions {
    pub native: bool,
    /// Rust toolchain (rustup/cargo/rustc) baked system-wide (INCLUDE_RUST).
    pub rust: bool,
    pub gpu: bool,
    pub ffmpeg: bool,
    /// Install the glint terminal-dashboard app (feeds INCLUDE_GLINT).
    pub glint: bool,
    /// Provider names to install (the checked agent CLIs). Feeds INSTALL_PROVIDERS.
    pub providers: Vec<String>,
}

/// Restores the terminal on drop (cooked mode + main screen + cursor), so a
/// panic or early error inside a fullscreen picker can't leave the shell "half
/// in, half out". Create it right after `enable_raw_mode()`; restoring twice on
/// the clean path is harmless.
struct ScreenGuard;

impl Drop for ScreenGuard {
    fn drop(&mut self) {
        disable_raw_mode().ok();
        execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture).ok();
        execute!(io::stdout(), crossterm::cursor::Show).ok();
    }
}

/// Ubuntu-installer-style checkbox screen for a bare `n8 build` on a terminal:
/// toggle the optional heavyweight layers + which agent CLIs to install with
/// space, then ⏎ to start the build. Returns the chosen options, or `None` if
/// the user cancels (esc/q → abort). `native`/`gpu`/`ffmpeg` are the initial
/// layer states (defaults: native on, the rest off); `available_providers` are
/// the installable agent CLIs, all checked on by default.
pub fn pick_build_options(
    native: bool,
    rust: bool,
    gpu: bool,
    ffmpeg: bool,
    glint: bool,
    available_providers: &[String],
) -> Result<Option<BuildOptions>> {
    // (label, size hint, checked)
    let mut checks: [(&str, &str, bool); 5] = [
        (
            "C/C++ build toolchain — gcc/make + headers (build C / node-gyp, link Rust)",
            "+300 MB",
            native,
        ),
        (
            "Rust toolchain — rustup/cargo/rustc, system-wide (agents can cargo build; pairs with the C toolchain for linking)",
            "+900 MB",
            rust,
        ),
        (
            "NVIDIA GPU support — CUDA runtime + cuDNN (then run with `n8 --gpu`)",
            "+3.6 GB",
            gpu,
        ),
        ("ffmpeg — static build", "+80 MB", ffmpeg),
        ("glint — terminal dashboard app (run from New → Type: App)", "+15 MB", glint),
    ];
    // Agent CLIs: (name, checked) — all on by default.
    let mut provs: Vec<(String, bool)> = available_providers
        .iter()
        .map(|p| (p.clone(), true))
        .collect();
    // Selectable rows: 0..3 layer checkboxes, 3..3+N provider checkboxes, then
    // 3+N = the "Start build" button.
    let start_row = checks.len() + provs.len();
    let mut sel: usize = 0;

    enable_raw_mode()?;
    let _guard = ScreenGuard; // restores on any exit (panic / early error included)
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result: Result<Option<BuildOptions>> = (|| {
        loop {
            terminal.draw(|f| {
                let area = f.area();
                let chunks = Layout::vertical([
                    Constraint::Length(2), // header
                    Constraint::Min(1),    // checkbox list + button
                    Constraint::Length(1), // footer
                ])
                .split(area.inner(Margin::new(2, 1)));

                f.render_widget(
                    Paragraph::new(vec![
                        Line::from(Span::styled(
                            "n8 build — select image layers",
                            Style::default()
                                .fg(Color::White)
                                .add_modifier(Modifier::BOLD),
                        )),
                        Line::from(Span::styled(
                            "On top of the pulled base image. Toggle what to bake in.",
                            Style::default().fg(Color::Indexed(244)),
                        )),
                    ]),
                    chunks[0],
                );

                let mut lines: Vec<Line> = Vec::new();
                for (i, (label, size, checked)) in checks.iter().enumerate() {
                    let selected = sel == i;
                    let boxs = if *checked { "[x]" } else { "[ ]" };
                    let base = if selected {
                        Style::default().bg(Color::Indexed(238)).fg(Color::White)
                    } else if *checked {
                        Style::default().fg(Color::White)
                    } else {
                        Style::default().fg(Color::Gray)
                    };
                    lines.push(Line::from(vec![
                        Span::styled(format!(" {boxs}  "), base),
                        Span::styled((*label).to_string(), base),
                        Span::raw("  "),
                        Span::styled((*size).to_string(), Style::default().fg(Color::Indexed(244))),
                    ]));
                }
                lines.push(Line::from(""));
                // Agent CLIs group (which coding agents to bake into the image).
                lines.push(Line::from(Span::styled(
                    "  Agent CLIs to install (uncheck to skip):",
                    Style::default().fg(Color::Indexed(244)),
                )));
                for (j, (name, checked)) in provs.iter().enumerate() {
                    let selected = sel == checks.len() + j;
                    let boxs = if *checked { "[x]" } else { "[ ]" };
                    let base = if selected {
                        Style::default().bg(Color::Indexed(238)).fg(Color::White)
                    } else if *checked {
                        Style::default().fg(Color::White)
                    } else {
                        Style::default().fg(Color::Gray)
                    };
                    lines.push(Line::from(vec![
                        Span::styled(format!(" {boxs}  "), base),
                        Span::styled(name.clone(), base),
                    ]));
                }
                lines.push(Line::from(""));
                let start_sel = sel == start_row;
                let start_style = if start_sel {
                    Style::default()
                        .bg(Color::Green)
                        .fg(Color::Black)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD)
                };
                lines.push(Line::from(Span::styled("  [ Start build ]  ", start_style)));
                f.render_widget(Paragraph::new(lines), chunks[1]);

                f.render_widget(
                    Paragraph::new(Line::from(vec![
                        Span::styled("↑↓", Style::default().fg(Color::Yellow)),
                        Span::raw(" move   "),
                        Span::styled("space", Style::default().fg(Color::Yellow)),
                        Span::raw(" toggle   "),
                        Span::styled("⏎", Style::default().fg(Color::Yellow)),
                        Span::raw(" start build   "),
                        Span::styled("esc", Style::default().fg(Color::Yellow)),
                        Span::raw(" cancel"),
                    ]))
                    .style(Style::default().fg(Color::Gray)),
                    chunks[2],
                );
            })?;

            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => return Ok(None),
                    KeyCode::Up | KeyCode::Char('k') => sel = sel.saturating_sub(1),
                    KeyCode::Down | KeyCode::Char('j') => sel = (sel + 1).min(start_row),
                    KeyCode::Char(' ') => {
                        // Toggle the current entry, remembering whether it WAS checked.
                        let was_checked = if sel < checks.len() {
                            let prev = checks[sel].2;
                            checks[sel].2 = !prev;
                            prev
                        } else if sel < start_row {
                            let j = sel - checks.len();
                            let prev = provs[j].1;
                            provs[j].1 = !prev;
                            prev
                        } else {
                            false
                        };
                        // On an UNCHECK, jump to the next still-checked entry so you
                        // can clear a run of them with repeated space presses.
                        if was_checked {
                            let is_checked = |i: usize| {
                                if i < checks.len() {
                                    checks[i].2
                                } else if i < start_row {
                                    provs[i - checks.len()].1
                                } else {
                                    false
                                }
                            };
                            if let Some(next) = ((sel + 1)..start_row).find(|&i| is_checked(i)) {
                                sel = next;
                            }
                        }
                    }
                    KeyCode::Enter => {
                        return Ok(Some(BuildOptions {
                            native: checks[0].2,
                            rust: checks[1].2,
                            gpu: checks[2].2,
                            ffmpeg: checks[3].2,
                            glint: checks[4].2,
                            providers: provs
                                .iter()
                                .filter(|(_, c)| *c)
                                .map(|(n, _)| n.clone())
                                .collect(),
                        }));
                    }
                    _ => {}
                }
            }
        }
    })();

    disable_raw_mode().ok();
    execute!(terminal.backend_mut(), LeaveAlternateScreen).ok();
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
                        KeyCode::Enter => {
                            filtering = false;
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
    let provider = s.provider.clone().unwrap_or_else(|| "-".into());
    let started = crate::session::compact_time(s.created.as_deref());
    let ran = crate::session::duration_str(s.created.as_deref(), s.modified.as_deref());
    let size = format_size(s.size_bytes);
    let workspace = crate::session::display_workspace(s.workspace.as_deref());

    Line::from(vec![
        // Full session UUID.
        Span::styled(format!("{:<36}  ", s.id), Style::default().fg(Color::Cyan)),
        Span::styled(format!("{provider:<12}  "), Style::default().fg(Color::Green)),
        Span::raw(format!("{started:<12}  ")),
        Span::styled(format!("{ran:>6}  "), Style::default().fg(Color::Indexed(244))),
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
