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
}

impl LogPane {
    pub fn new(index: EventIndex) -> Self {
        let kind_cycle = index.facets().keys().cloned().collect();
        Self {
            index,
            query_text: String::new(),
            active_kind: None,
            kind_cycle,
            sel: 0,
            focus: Focus::Search,
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
            Focus::Search => Focus::List,
            Focus::List => Focus::Search,
        };
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
        // Build a ring: index 0 = "all" (None), 1..=n = kinds.
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

/// `HH:MM:SS` for a unix-seconds timestamp (UTC).
fn fmt_time(ts: u64) -> String {
    chrono::DateTime::from_timestamp(ts as i64, 0)
        .map(|dt| dt.format("%H:%M:%S").to_string())
        .unwrap_or_else(|| "--:--:--".to_string())
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
                "cpu {:.0}%  mem {}MB  load {:.2}",
                f("cpu_pct"),
                u("mem_used_kb") / 1024,
                f("load1")
            )
        }
        "heartbeat" => format!("pid {}", e.raw.get("pid").and_then(|v| v.as_u64()).unwrap_or(0)),
        "net" => format!("{} {}:{}", g("protocol"), g("dest"), e.raw.get("port").and_then(|v| v.as_u64()).unwrap_or(0)),
        _ => g("msg").to_string(),
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

/// A list row: `HH:MM:SS  kind        summary`.
pub fn format_row(e: &IndexedEvent) -> String {
    format!("{}  {:<10} {}", fmt_time(e.ts), e.kind, summary(e))
}

/// Render the panel into `area`. Layout: search bar (top) · [facets | list]
/// (middle) · detail (bottom).
pub fn draw(f: &mut Frame, area: Rect, pane: &LogPane) {
    let chunks = Layout::vertical([
        Constraint::Length(3), // search bar
        Constraint::Min(5),    // facets | list
        Constraint::Length(7), // detail
    ])
    .split(area);

    // ── search bar ──
    let search_focused = pane.focus == Focus::Search;
    let kind_label = pane.active_kind().unwrap_or("all");
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
    ]);
    f.render_widget(
        Paragraph::new(bar).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" LOGPANE ")
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

    let mut facet_items: Vec<ListItem> = Vec::new();
    let total: usize = pane.index.len();
    facet_items.push(ListItem::new(format!(
        "{} all ({total})",
        if pane.active_kind().is_none() { "▸" } else { " " }
    )));
    for (k, c) in pane.index.facets() {
        let marker = if pane.active_kind() == Some(k.as_str()) { "▸" } else { " " };
        facet_items.push(ListItem::new(format!("{marker} {k} ({c})")));
    }
    f.render_widget(
        List::new(facet_items).block(Block::default().borders(Borders::ALL).title(" kinds ")),
        mid[0],
    );

    let visible = pane.visible();
    let rows: Vec<ListItem> = visible
        .iter()
        .map(|e| ListItem::new(format_row(e)))
        .collect();
    let list_focused = pane.focus == Focus::List;
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
}

/// Interactive panel over a session's `events.jsonl`. Type to filter, `Tab`
/// toggles search/list focus, `←/→` cycle the kind facet, `↑/↓` move, `Esc`/`q`
/// quits. Reloads the index on `r`.
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

    let res = (|| -> std::io::Result<()> {
        loop {
            terminal.draw(|f| draw(f, f.area(), &pane))?;
            if !event::poll(std::time::Duration::from_millis(500))? {
                continue;
            }
            if let Event::Key(k) = event::read()? {
                if k.kind != KeyEventKind::Press {
                    continue;
                }
                match k.code {
                    KeyCode::Esc => break,
                    KeyCode::Char('q') if pane.focus() == Focus::List => break,
                    KeyCode::Tab => pane.toggle_focus(),
                    KeyCode::Left => pane.cycle_kind(-1),
                    KeyCode::Right => pane.cycle_kind(1),
                    KeyCode::Up => pane.move_sel(-1),
                    KeyCode::Down => pane.move_sel(1),
                    KeyCode::Char('r') => {
                        pane = LogPane::new(EventIndex::load_jsonl(&jsonl_path, cap));
                    }
                    KeyCode::Backspace => pane.backspace(),
                    KeyCode::Char(c) if pane.focus() == Focus::Search => pane.push_char(c),
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
        assert!(content.contains("LOGPANE"));
        assert!(content.contains("events"));
    }
}
