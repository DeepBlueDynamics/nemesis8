//! LOGPANE EPIC 3 — the Splunk/Loggly-style search UI. A ratatui panel over the
//! event index (EPIC 2): a live search bar, a kind-facet sidebar, a newest-first
//! event list, and a detail pane. Reuses our existing ratatui stack — no web
//! server. The interactive [`run`] loop is a thin shell; the panel STATE
//! ([`LogPane`]) and the [`draw`] renderer are pure and unit-tested.

use crate::event_index::{EventIndex, EventQuery, IndexedEvent};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;

/// Which control has keyboard focus.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Focus {
    Search,
    Kinds,
    List,
}

/// Panel state: the loaded index plus the live query (text + kind facet) and the
/// list selection. All mutation goes through methods so the query and selection
/// stay consistent (any filter change resets the selection to the newest row).
pub struct LogPane {
    index: EventIndex,
    query_text: String,
    /// Active kind facet; `None` = all kinds.
    active_kind: Option<String>,
    /// Facet kinds to cycle through (sorted), built from the index at construction.
    kind_cycle: Vec<String>,
    sel: usize,
    focus: Focus,
    pub live: bool,
    pub show_detail_modal: bool,
    pub detail_scroll: u16,
}

impl LogPane {
    pub fn new(index: EventIndex) -> Self {
        let mut kind_cycle: Vec<String> = index.facets().keys().cloned().collect();
        kind_cycle.sort();
        Self {
            index,
            query_text: String::new(),
            active_kind: None,
            kind_cycle,
            sel: 0,
            focus: Focus::Search,
            live: true,
            show_detail_modal: false,
            detail_scroll: 0,
        }
    }

    fn query(&self) -> EventQuery {
        EventQuery {
            kinds: self.active_kind.iter().cloned().collect(),
            text: if self.query_text.is_empty() {
                None
            } else {
                Some(self.query_text.clone())
            },
            ..Default::default()
        }
    }

    /// Events matching the current query, newest-first.
    pub fn visible(&self) -> Vec<&IndexedEvent> {
        self.index.query(&self.query())
    }

    pub fn focus(&self) -> Focus {
        self.focus
    }

    pub fn toggle_focus(&mut self) {
        self.focus = match self.focus {
            Focus::Search => Focus::Kinds,
            Focus::Kinds => Focus::List,
            Focus::List => Focus::Search,
        };
    }

    pub fn toggle_live(&mut self) {
        self.live = !self.live;
    }

    pub fn scroll_detail(&mut self, delta: i16) {
        if delta < 0 {
            self.detail_scroll = self.detail_scroll.saturating_sub(delta.unsigned_abs());
        } else {
            self.detail_scroll = self.detail_scroll.saturating_add(delta.unsigned_abs());
        }
    }

    pub fn push_char(&mut self, c: char) {
        self.query_text.push(c);
        self.sel = 0;
    }

    pub fn backspace(&mut self) {
        self.query_text.pop();
        self.sel = 0;
    }

    /// Cycle the kind facet: `None → kinds[0] → … → kinds[n-1] → None`.
    pub fn cycle_kind(&mut self, dir: i32) {
        if self.kind_cycle.is_empty() {
            return;
        }
        let n = self.kind_cycle.len() as i32 + 1;
        let cur = match &self.active_kind {
            None => 0,
            Some(k) => self
                .kind_cycle
                .iter()
                .position(|x| x == k)
                .map(|p| p as i32 + 1)
                .unwrap_or(0),
        };
        let next = (cur + dir).rem_euclid(n);
        self.active_kind = if next == 0 {
            None
        } else {
            Some(self.kind_cycle[(next - 1) as usize].clone())
        };
        self.sel = 0;
    }

    pub fn move_kind_sel(&mut self, delta: i32) {
        self.cycle_kind(delta);
    }

    pub fn move_sel(&mut self, delta: i32) {
        let n = self.visible().len();
        if n == 0 {
            self.sel = 0;
            return;
        }
        let max = (n - 1) as i32;
        let cur = (self.sel as i32).min(max);
        self.sel = (cur + delta).clamp(0, max) as usize;
    }

    /// The currently-selected event (clamped), if any.
    pub fn selected(&self) -> Option<IndexedEvent> {
        self.visible().get(self.sel).map(|e| (*e).clone())
    }

    pub fn active_kind(&self) -> Option<&str> {
        self.active_kind.as_deref()
    }
}

/// Full Zulu timestamp (UTC).
fn fmt_time(ts: u64) -> String {
    chrono::DateTime::from_timestamp(ts as i64, 0)
        .map(|dt| dt.format("%Y-%m-%dT%H:%M:%SZ").to_string())
        .unwrap_or_else(|| "0000-00-00T00:00:00Z".to_string())
}

/// A one-line, human-readable summary of an event — the most relevant string(s)
/// for its kind. Falls back to the raw search-relevant fields for unknown kinds.
pub fn summary(e: &IndexedEvent) -> String {
    let g = |k: &str| e.raw.get(k).and_then(|v| v.as_str()).unwrap_or("");
    match e.kind.as_str() {
        "log_line" => format!("{}  {}", short_path(g("path")), g("line")),
        "fs" => format!("{} {}", g("kind_detail"), short_path(g("path"))),
        "status" => format!("{} {}", g("status"), g("msg")),
        "metric" => {
            let f = |k: &str| e.raw.get(k).and_then(|v| v.as_f64()).unwrap_or(0.0);
            let u = |k: &str| e.raw.get(k).and_then(|v| v.as_u64()).unwrap_or(0);
            format!(
                "cpu {:.0}%  mem {}MB  load {:.2}  net ↓{}/s ↑{}/s",
                f("cpu_pct"),
                u("mem_used_kb") / 1024,
                f("load1"),
                human_bytes(u("net_rx_bps")),
                human_bytes(u("net_tx_bps")),
            )
        }
        "heartbeat" => format!("pid {}", e.raw.get("pid").and_then(|v| v.as_u64()).unwrap_or(0)),
        "net" => format!("{} {}:{}", g("protocol"), g("dest"), e.raw.get("port").and_then(|v| v.as_u64()).unwrap_or(0)),
        _ => g("msg").to_string(),
    }
}

/// Compact byte count: `0B` / `512B` / `4.2KB` / `1.5MB`.
fn human_bytes(n: u64) -> String {
    if n < 1024 {
        format!("{n}B")
    } else if n < 1024 * 1024 {
        format!("{:.1}KB", n as f64 / 1024.0)
    } else {
        format!("{:.1}MB", n as f64 / (1024.0 * 1024.0))
    }
}

/// Trim a long absolute path to its trailing two components for the list.
fn short_path(p: &str) -> String {
    let parts: Vec<&str> = p.rsplit('/').take(2).collect();
    if parts.len() == 2 {
        format!(".../{}/{}", parts[1], parts[0])
    } else {
        p.to_string()
    }
}

/// A list row: `YYYY-MM-DDTHH:MM:SSZ  kind        summary`.
pub fn format_row(e: &IndexedEvent) -> String {
    format!("{}  {:<10}  {}", fmt_time(e.ts), e.kind, summary(e))
}

fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::vertical([
        Constraint::Percentage((100 - percent_y) / 2),
        Constraint::Percentage(percent_y),
        Constraint::Percentage((100 - percent_y) / 2),
    ])
    .split(r);

    Layout::horizontal([
        Constraint::Percentage((100 - percent_x) / 2),
        Constraint::Percentage(percent_x),
        Constraint::Percentage((100 - percent_x) / 2),
    ])
    .split(popup_layout[1])[1]
}

/// Render the panel into `area`. Layout: search bar (top) · [facets | list]
/// (middle) · detail (bottom) · help (very bottom).
pub fn draw(f: &mut Frame, area: Rect, pane: &LogPane) {
    let chunks = Layout::vertical([
        Constraint::Length(3), // search bar
        Constraint::Min(5),    // facets | list
        Constraint::Length(7), // detail
        Constraint::Length(1), // help bar
    ])
    .split(area);

    // ── search bar ──
    let search_focused = pane.focus == Focus::Search;
    let kind_label = pane.active_kind().unwrap_or("all");
    let status_badge = if pane.live {
        Span::styled(" [LIVE] ", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD))
    } else {
        Span::styled(" [PAUSED] ", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD))
    };
    let bar = Line::from(vec![
        Span::styled(" search ", Style::default().fg(Color::Black).bg(Color::Cyan)),
        Span::raw(" "),
        Span::styled(
            if pane.query_text.is_empty() { "<type to filter>" } else { &pane.query_text },
            if pane.query_text.is_empty() {
                Style::default().fg(Color::DarkGray)
            } else {
                Style::default().fg(Color::White)
            },
        ),
        Span::raw("   "),
        Span::styled(format!("kind: {kind_label}"), Style::default().fg(Color::Yellow)),
        Span::raw("   "),
        status_badge,
    ]);
    f.render_widget(
        Paragraph::new(bar).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" NEMESIS8 LOG ")
                .border_style(if search_focused {
                    Style::default().fg(Color::Cyan)
                } else {
                    Style::default().fg(Color::DarkGray)
                }),
        ),
        chunks[0],
    );

    // ── middle: facets | list ──
    let mid = Layout::horizontal([Constraint::Length(22), Constraint::Min(20)]).split(chunks[1]);

    let kinds_focused = pane.focus == Focus::Kinds;
    let mut facet_items: Vec<ListItem> = Vec::new();
    let total: usize = pane.index.len();
    let is_all_active = pane.active_kind().is_none();
    let all_style = if is_all_active {
        Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Gray)
    };
    facet_items.push(ListItem::new(format!(
        "{} all ({total})",
        if is_all_active { "▸" } else { " " }
    )).style(all_style));

    for (k, c) in pane.index.facets() {
        let is_active = pane.active_kind() == Some(k.as_str());
        let item_style = if is_active {
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Gray)
        };
        let marker = if is_active { "▸" } else { " " };
        facet_items.push(ListItem::new(format!("{marker} {k} ({c})")).style(item_style));
    }
    f.render_widget(
        List::new(facet_items).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" kinds ")
                .border_style(if kinds_focused {
                    Style::default().fg(Color::Cyan)
                } else {
                    Style::default().fg(Color::DarkGray)
                }),
        ),
        mid[0],
    );

    let visible = pane.visible();
    let list_focused = pane.focus == Focus::List;
    let rows: Vec<ListItem> = visible
        .iter()
        .enumerate()
        .map(|(idx, e)| {
            let mut spans = Vec::new();
            spans.push(Span::styled(format!("{} ", fmt_time(e.ts)), Style::default().fg(Color::DarkGray)));
            
            let kind_color = match e.kind.as_str() {
                "log_line" => Color::Green,
                "fs" => Color::Cyan,
                "status" => Color::Blue,
                "metric" => Color::Yellow,
                "heartbeat" => Color::Magenta,
                "net" => Color::Red,
                _ => Color::White,
            };
            spans.push(Span::styled(format!(" {:<10} ", e.kind), Style::default().fg(kind_color).add_modifier(Modifier::BOLD)));
            spans.push(Span::styled(summary(e), Style::default().fg(Color::White)));
            
            let is_sel = list_focused && idx == pane.sel;
            let item_style = if is_sel {
                Style::default().bg(Color::Rgb(40, 40, 50))
            } else {
                Style::default()
            };
            ListItem::new(Line::from(spans)).style(item_style)
        })
        .collect();

    let mut lstate = ListState::default();
    if !visible.is_empty() {
        lstate.select(Some(pane.sel.min(visible.len() - 1)));
    }
    f.render_stateful_widget(
        List::new(rows)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!(" events ({}) ", visible.len()))
                    .border_style(if list_focused {
                        Style::default().fg(Color::Cyan)
                    } else {
                        Style::default().fg(Color::DarkGray)
                    }),
            )
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED)),
        mid[1],
        &mut lstate,
    );

    // ── detail ──
    let detail = pane
        .selected()
        .map(|e| serde_json::to_string_pretty(&e.raw).unwrap_or_default())
        .unwrap_or_else(|| "no events match — clear the filter or wait for activity".to_string());
    f.render_widget(
        Paragraph::new(detail)
            .wrap(Wrap { trim: false })
            .block(Block::default().borders(Borders::ALL).title(" detail ")),
        chunks[2],
    );

    // ── help bar ──
    let help_line = Line::from(vec![
        Span::styled(" [Esc] ", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
        Span::raw("Quit  "),
        Span::styled(" [Tab] ", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
        Span::raw("Focus  "),
        Span::styled(" [Space] ", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
        Span::raw(if pane.live { "Pause [LIVE]  " } else { "Resume [PAUSED]  " }),
        Span::styled(" [Enter] ", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
        Span::raw("Detail Modal  "),
        Span::styled(" [r] ", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
        Span::raw("Reload  "),
        Span::styled(" [↑/↓] ", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
        Span::raw("Navigate"),
    ]);
    f.render_widget(Paragraph::new(help_line), chunks[3]);

    // ── detail modal overlay ──
    if pane.show_detail_modal {
        let modal_area = centered_rect(80, 80, area);
        f.render_widget(ratatui::widgets::Clear, modal_area);
        let detail_text = pane
            .selected()
            .map(|e| serde_json::to_string_pretty(&e.raw).unwrap_or_default())
            .unwrap_or_else(|| "No event selected".to_string());
        f.render_widget(
            Paragraph::new(detail_text)
                .wrap(Wrap { trim: false })
                .scroll((pane.detail_scroll, 0))
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(" EVENT DETAIL (Press Enter/Esc to close, Up/Down to scroll) ")
                        .border_style(Style::default().fg(Color::Cyan)),
                ),
            modal_area,
        );
    }
}

/// Interactive panel over a session's `events.jsonl`. Type to filter, `Tab`
/// toggles search/kinds/list focus, `↑/↓` moves, `Esc`/`q` quits, `Space` plays/pauses.
/// Reloads index automatically every 1s when live.
pub fn run(jsonl_path: std::path::PathBuf, cap: usize) -> std::io::Result<()> {
    use crossterm::event::{self, Event, KeyCode, KeyEventKind};
    use crossterm::execute;
    use crossterm::terminal::{
        disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
    };
    use ratatui::backend::CrosstermBackend;
    use ratatui::Terminal;

    let mut pane = LogPane::new(EventIndex::load_jsonl(&jsonl_path, cap));

    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut last_reload = std::time::Instant::now();

    let res = (|| -> std::io::Result<()> {
        loop {
            // Auto-reload every 1 second if live tailing is enabled
            if pane.live && last_reload.elapsed() >= std::time::Duration::from_secs(1) {
                let prev_sel = pane.sel;
                let prev_focus = pane.focus;
                let prev_active_kind = pane.active_kind.clone();
                let prev_query_text = pane.query_text.clone();
                let prev_show_modal = pane.show_detail_modal;
                let prev_modal_scroll = pane.detail_scroll;
                
                pane = LogPane::new(EventIndex::load_jsonl(&jsonl_path, cap));
                pane.sel = prev_sel;
                pane.focus = prev_focus;
                pane.active_kind = prev_active_kind;
                pane.query_text = prev_query_text;
                pane.show_detail_modal = prev_show_modal;
                pane.detail_scroll = prev_modal_scroll;
                
                last_reload = std::time::Instant::now();
            }

            terminal.draw(|f| draw(f, f.area(), &pane))?;
            
            // Short poll (250ms) to allow responsive real-time reload checking
            if !event::poll(std::time::Duration::from_millis(250))? {
                continue;
            }
            if let Event::Key(k) = event::read()? {
                if k.kind != KeyEventKind::Press {
                    continue;
                }
                match k.code {
                    KeyCode::Esc => {
                        if pane.show_detail_modal {
                            pane.show_detail_modal = false;
                        } else {
                            break;
                        }
                    }
                    KeyCode::Char('q') => {
                        if pane.show_detail_modal {
                            pane.show_detail_modal = false;
                        } else if pane.focus() == Focus::List || pane.focus() == Focus::Kinds {
                            break;
                        }
                    }
                    KeyCode::Enter => {
                        if pane.show_detail_modal {
                            pane.show_detail_modal = false;
                        } else {
                            pane.show_detail_modal = true;
                            pane.detail_scroll = 0;
                        }
                    }
                    KeyCode::Tab => {
                        if !pane.show_detail_modal {
                            pane.toggle_focus();
                        }
                    }
                    KeyCode::Up => {
                        if pane.show_detail_modal {
                            pane.scroll_detail(-1);
                        } else {
                            match pane.focus() {
                                Focus::Search => {},
                                Focus::List => pane.move_sel(-1),
                                Focus::Kinds => pane.move_kind_sel(-1),
                            }
                        }
                    }
                    KeyCode::Down => {
                        if pane.show_detail_modal {
                            pane.scroll_detail(1);
                        } else {
                            match pane.focus() {
                                Focus::Search => {},
                                Focus::List => pane.move_sel(1),
                                Focus::Kinds => pane.move_kind_sel(1),
                            }
                        }
                    }
                    KeyCode::Char(' ') => {
                        if !pane.show_detail_modal {
                            pane.toggle_live();
                        }
                    }
                    KeyCode::Left => {
                        if !pane.show_detail_modal {
                            pane.cycle_kind(-1);
                        }
                    }
                    KeyCode::Right => {
                        if !pane.show_detail_modal {
                            pane.cycle_kind(1);
                        }
                    }
                    KeyCode::Char('r') => {
                        let prev_sel = pane.sel;
                        let prev_focus = pane.focus;
                        let prev_active_kind = pane.active_kind.clone();
                        let prev_query_text = pane.query_text.clone();
                        
                        pane = LogPane::new(EventIndex::load_jsonl(&jsonl_path, cap));
                        pane.sel = prev_sel;
                        pane.focus = prev_focus;
                        pane.active_kind = prev_active_kind;
                        pane.query_text = prev_query_text;
                        
                        last_reload = std::time::Instant::now();
                    }
                    KeyCode::Backspace => {
                        if !pane.show_detail_modal && pane.focus() == Focus::Search {
                            pane.backspace();
                        }
                    }
                    KeyCode::Char(c) if pane.focus() == Focus::Search && !pane.show_detail_modal => {
                        pane.push_char(c);
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    })();

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    res
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn pane() -> LogPane {
        let mut i = EventIndex::new(100);
        i.ingest_value(json!({"kind":"log_line","ts":10,"path":"/workspace/app.log","line":"ERROR boom"}));
        i.ingest_value(json!({"kind":"log_line","ts":20,"path":"/workspace/app.log","line":"all good"}));
        i.ingest_value(json!({"kind":"metric","ts":15,"cpu_pct":42.0,"mem_used_kb":2048,"load1":0.5}));
        i.ingest_value(json!({"kind":"fs","ts":25,"path":"/workspace/src/main.rs","kind_detail":"modified"}));
        LogPane::new(i)
    }

    #[test]
    fn text_filter_narrows_and_resets_selection() {
        let mut p = pane();
        p.move_sel(2);
        for c in "error".chars() {
            p.push_char(c);
        }
        let v = p.visible();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].ts, 10);
        assert_eq!(p.selected().unwrap().ts, 10); // selection reset to row 0
    }

    #[test]
    fn kind_cycle_rings_through_all_then_none() {
        let mut p = pane();
        assert_eq!(p.active_kind(), None);
        // facets sorted: fs, log_line, metric
        p.cycle_kind(1);
        assert_eq!(p.active_kind(), Some("fs"));
        p.cycle_kind(1);
        assert_eq!(p.active_kind(), Some("log_line"));
        assert_eq!(p.visible().len(), 2);
        p.cycle_kind(-1);
        assert_eq!(p.active_kind(), Some("fs"));
        p.cycle_kind(-1);
        assert_eq!(p.active_kind(), None); // wrap back to "all"
    }

    #[test]
    fn selection_clamps() {
        let mut p = pane();
        p.move_sel(-5);
        assert_eq!(p.selected().unwrap().ts, 25); // newest, clamped at top
        p.move_sel(100);
        assert_eq!(p.selected().unwrap().ts, 10); // oldest, clamped at bottom
    }

    #[test]
    fn summaries_are_readable() {
        let p = pane();
        let v = p.visible();
        let metric = v.iter().find(|e| e.kind == "metric").unwrap();
        assert!(summary(metric).contains("cpu 42%"));
        let fs = v.iter().find(|e| e.kind == "fs").unwrap();
        assert!(summary(fs).contains("modified"));
    }

    #[test]
    fn draws_without_panicking() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let p = pane();
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, f.area(), &p)).unwrap();
        let content = terminal.backend().buffer().content().iter().map(|c| c.symbol()).collect::<String>();
        assert!(content.contains("NEMESIS8 LOG"));
        assert!(content.contains("events"));
    }
}
