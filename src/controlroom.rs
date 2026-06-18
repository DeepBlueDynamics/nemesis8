//! n8 control room — what bare `n8` opens.
//!
//! A Microsoft-`edit`-style TUI: a top **menu bar** (Session / Fleet /
//! Container / Tools / Config / Help) over two **tabs** — Running (attach) and
//! Sessions (resume) — each a scrollable table showing session id + workspace.
//! Full keyboard *and* mouse (click menus/tabs/rows, wheel-scroll).
//!
//! Returns a [`PickAction`] (Attach / Resume / New) or None on quit. The menu's
//! Session items drive in-TUI actions; the other menus are the discoverability
//! outline of n8's functions (they surface the `n8 <cmd>` to run).

use anyhow::Result;
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
        MouseButton, MouseEventKind,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, Borders, Cell, Clear, Paragraph, Row, Scrollbar, ScrollbarOrientation,
        ScrollbarState, Table, TableState, Tabs,
    },
    Terminal,
};
use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};

use crate::picker::RunningAgent;
use crate::session::SessionInfo;

/// What the control room resolved to. Decoupled from picker::PickAction so the
/// New-session modal can carry its provider/model/danger choice straight out.
pub enum Outcome {
    Attach(String),
    Resume {
        session: SessionInfo,
        current_dir: bool,
    },
    NewSession {
        provider: String,
        model: Option<String>,
        danger: bool,
    },
    /// Rebuild the Docker image (Config → Build image). The control room exits
    /// and main.rs runs the build flow on the now-free terminal.
    Build,
}

/// Menu titles and their items. Session items (menu 0) are wired to in-TUI
/// actions by index; every other item is a discoverability hint (the text in
/// parens is the shell command to run).
// Per Law 1 (every menu item DOES something in the pane), only menus whose
// items are functional in-TUI are listed. Fleet / Container / Tools / Config
// return as real in-pane views in the next pass — they are NOT stubbed here.
const MENUS: &[(&str, &[&str])] = &[
    // Only distinct in-pane actions. Resume/Attach/List were just tab
    // navigation (the tabs already do that) — dropped. "Search sessions"
    // (all-saved content search) returns when its in-pane view ships.
    ("Session", &["New session", "Find"]),
    ("Config", &["Edit tools", "Build image", "Validate config", "Init config", "Archive & reset"]),
    ("Help", &["Keys", "About"]),
];

/// Where a key hint shows: the TOP action bar or the BOTTOM nav bar. Each key
/// lives in exactly one (no top/bottom duplication).
#[derive(Clone, Copy, PartialEq)]
enum Bar {
    Top,
    Bot,
}

/// Single source of truth for key hints — drives the top action bar, the bottom
/// nav bar, AND Help ▸ Keys. Edit here and all three update. (key, what, where)
const KEYS: &[(&str, &str, Bar)] = &[
    ("n", "new", Bar::Top),
    ("t", "tools", Bar::Top),
    ("⏎", "open", Bar::Top),
    ("a", "attach/resume", Bar::Top),
    (".", "resume here", Bar::Top),
    ("k", "kill", Bar::Top),
    ("l", "logs", Bar::Top),
    ("r", "refresh", Bar::Top),
    ("/", "find", Bar::Top),
    ("Tab", "tabs", Bar::Top),
    ("q", "quit", Bar::Top),
    ("↑↓", "move", Bar::Bot),
    ("PgUp/PgDn", "page", Bar::Bot),
    ("Home/End", "ends", Bar::Bot),
    ("Alt+S/H", "menus", Bar::Bot),
];

fn bar_line(which: Bar) -> Line<'static> {
    let mut spans = vec![Span::raw(" ")];
    let mut first = true;
    for (k, what, w) in KEYS.iter() {
        if *w != which {
            continue;
        }
        if !first {
            spans.push(Span::styled(" · ", Style::default().fg(Color::DarkGray)));
        }
        first = false;
        spans.push(Span::styled(*k, Style::default().fg(Color::Yellow)));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(*what, Style::default().fg(Color::Gray)));
    }
    Line::from(spans)
}

/// Rows a hint bar needs at `width` so hints WRAP on narrow terminals instead
/// of clipping. Clamped: never more than 3 rows of chrome per bar.
fn bar_height(which: Bar, width: u16) -> u16 {
    let len: usize = bar_line(which)
        .spans
        .iter()
        .map(|s| s.content.chars().count())
        .sum();
    let w = width.max(1) as usize;
    (((len + w - 1) / w) as u16).clamp(1, 3)
}

/// A field in the New-session modal.
#[derive(Clone, Copy, PartialEq)]
enum MField {
    Provider,
    Model,
    Danger,
    Launch,
    Cancel,
}

/// New-session modal state.
struct NewModal {
    provider_idx: usize,
    model: String,
    danger: bool,
    focus: MField,
    dd_open: bool,    // provider pulldown open
    dd_sel: usize,    // highlighted provider in the pulldown
    mdd_open: bool,   // model pulldown open
    mdd_sel: usize,   // highlighted row in the model pulldown (0 = default)
}

/// Detail overlay state (v3 §3.4): sectioned, with scrollable logs.
struct Detail {
    /// 0 = Meta, 1 = Tools, 2 = Logs.
    section: u8,
    /// Last `docker logs --tail` capture for the selected agent.
    logs: Vec<String>,
    /// Lines scrolled UP from the tail. 0 = following the tail.
    scroll: usize,
}

/// Origin of a row in the tools picker — drives its tag and color.
#[derive(Clone, Copy, PartialEq)]
enum ToolKind {
    /// A `.py` present in the image's `/opt/mcp-source` (what actually loads).
    Builtin,
    /// A socket (HTTP/SSE) MCP server from the registry (`mcp-servers/*.toml` +
    /// user TOMLs) — toggling adds/removes its name in `mcp_tools`.
    Registry,
    /// An `http(s)` MCP endpoint already configured in `mcp_tools`.
    Url,
    /// Configured but not a built-in (host-only / stale) — shown so it can be removed.
    Extra,
    /// Always-on binary server (nuts-files) — informational, not toggleable.
    Binary,
    /// A `.py` sitting in the volume's `mcp/` junk drawer that the current image
    /// no longer ships (`/opt/mcp-source` lacks it). These are the orphans that
    /// surface as ghost servers — shown so they can be deleted from disk.
    Stale,
}

/// Tools picker: add/remove MCP tools for the workspace the next New / Resume
/// will use. Edits that workspace's `.nemesis8.toml` directly, persisted on
/// every toggle. Not reachable for Attach — a live container's tools are fixed
/// at boot, so changing them there would be a lie.
struct ToolsModal {
    /// `.nemesis8.toml` being edited (cwd for New, the session's workspace for Resume).
    target: PathBuf,
    /// Short workspace label for the title.
    target_label: String,
    /// Displayed rows: image built-ins ∪ configured extras, plus the binary header.
    rows: Vec<(String, ToolKind)>,
    /// Currently-enabled tool ids (the `mcp_tools` set on disk).
    enabled: HashSet<String>,
    /// Cursor into the *filtered* row list.
    sel: usize,
    /// Substring filter (the picker can hold 40+ tools).
    filter: String,
    filtering: bool,
    /// Transient feedback ("saved → …" / error).
    status: String,
    /// Tool name awaiting a delete confirmation (set by `d`, cleared by `y`/`n`).
    /// Deleting removes the `.py` from the volume's `mcp/` drawer (all
    /// workspaces) plus its antigravity schema-cache dir.
    confirm_delete: Option<String>,
    /// Add-a-socket-server overlay (opened with `a`). When Some, the picker's
    /// keys feed this form instead of the row list.
    adding: Option<AddServerInput>,
}

/// Add-server form state: name / url / optional bearer-token env var. On submit
/// it writes a registry TOML into the container-mapped user dir and enables the
/// server in the target workspace (issue #73).
struct AddServerInput {
    /// 0 = name, 1 = url, 2 = bearer-token env (optional).
    field: usize,
    name: String,
    url: String,
    token_env: String,
    /// Validation message shown under the form.
    error: String,
}

impl AddServerInput {
    fn new() -> Self {
        AddServerInput {
            field: 0,
            name: String::new(),
            url: String::new(),
            token_env: String::new(),
            error: String::new(),
        }
    }

    fn current_mut(&mut self) -> &mut String {
        match self.field {
            0 => &mut self.name,
            1 => &mut self.url,
            _ => &mut self.token_env,
        }
    }
}

/// Config-management overlay (the Config menu): inspect the active config, or
/// archive-and-reinit it. `mode` selects behavior; confirm modes act on Enter/`y`.
struct ConfigModal {
    mode: ConfigMode,
    /// The workspace `.nemesis8.toml` the actions target (cwd's).
    target: PathBuf,
    target_label: String,
    /// (tool name, resolves-to-something-real) for the Validate report.
    tools: Vec<(String, bool)>,
    /// Other `.nemesis8.toml` files that can shadow sessions (the home-root leak…).
    strays: Vec<PathBuf>,
    status: String,
    /// Set once a confirm action has run (so the modal shows the result).
    done: bool,
}

#[derive(PartialEq, Clone, Copy)]
enum ConfigMode {
    Validate,
    Init,
    Reset,
}

/// One model option from the /models endpoint.
#[derive(Clone, serde::Deserialize)]
pub struct ModelEntry {
    pub id: String,
    #[serde(default)]
    pub label: String,
}

/// A provider's model list from the /models endpoint.
#[derive(Clone, Default, serde::Deserialize)]
pub struct ProviderModels {
    #[serde(default)]
    pub ok: bool,
    #[serde(default)]
    pub default: Option<String>,
    #[serde(default)]
    pub models: Vec<ModelEntry>,
}

/// The whole /models response — drives the new-session model pulldown.
/// Degrades silently: absent/empty → the model field stays free-text.
#[derive(Clone, Default, serde::Deserialize)]
pub struct ModelCatalog {
    #[serde(default)]
    pub ttl_seconds: u64,
    #[serde(default)]
    pub providers: std::collections::HashMap<String, ProviderModels>,
}

/// Host-side context the control room needs to act (kill, logs, refresh)
/// without exiting the TUI.
pub struct Ctx {
    /// Container runtime binary ("docker"/"podman") for kill + logs.
    pub runtime: String,
    /// Active MCP tools (from config) for the detail Tools section.
    pub tools: Vec<String>,
    /// Ask the background refresher for fresh data right now.
    pub refresh_request: Option<tokio::sync::mpsc::UnboundedSender<()>>,
    /// Fresh running-agent lists pushed by the background refresher (~2s).
    pub updates: Option<std::sync::mpsc::Receiver<Vec<RunningAgent>>>,
    /// Model catalog, fetched once in the background (cached on disk).
    pub models: Option<std::sync::mpsc::Receiver<ModelCatalog>>,
    /// The cwd workspace's `.nemesis8.toml` path (target for New-session tool edits).
    pub config_path: PathBuf,
    /// Image built-in tool filenames (`/opt/mcp-source`), gathered in the
    /// background so the tools picker shows what will actually load.
    pub avail_tools: Option<std::sync::mpsc::Receiver<Vec<String>>>,
}

impl Default for Ctx {
    fn default() -> Self {
        Ctx {
            runtime: "docker".to_string(),
            tools: Vec::new(),
            refresh_request: None,
            updates: None,
            models: None,
            config_path: PathBuf::from(".nemesis8.toml"),
            avail_tools: None,
        }
    }
}

struct State {
    tab: usize,                 // 0 = Running, 1 = Sessions
    sel: [usize; 2],            // selected row per tab
    tstate: [TableState; 2],    // persistent so ratatui keeps scroll offset
    query: String,
    filtering: bool,
    menu_open: Option<usize>,   // which menu is dropped down
    menu_sel: usize,            // highlighted item in the open menu
    status: String,             // status-bar message (menu hints land here)
    menu_x: Vec<u16>,           // start column of each menu title (for clicks)
    detail: Option<Detail>,     // detail overlay for the selected row
    confirm_kill: Option<String>, // kill-confirm modal (agent name)
    pin_sel: Option<String>,    // re-pin selection to this agent after refresh
    help: Option<u8>,           // Help overlay: 1 = Keys, 2 = About
    modal: Option<NewModal>,    // New-session modal
    providers: Vec<String>,     // installed providers (for the pulldown)
    models: Option<ModelCatalog>, // per-provider model lists (when fetched)
    dflt_provider: usize,       // default provider index when opening the modal
    dflt_model: String,
    dflt_danger: bool,
    tools: Option<ToolsModal>,  // tools picker overlay (add/remove MCP tools)
    config: Option<ConfigModal>, // Config menu overlay (validate / init / reset)
    avail_tools: Vec<String>,   // image built-in tool filenames (from bg fetch)
    cwd_config: PathBuf,        // cwd workspace .nemesis8.toml (New-session target)
    provider_hints: HashMap<String, String>, // provider name (lc) → model-picker hint
}

impl State {
    fn open_modal(&mut self) {
        self.modal = Some(NewModal {
            provider_idx: self.dflt_provider,
            model: self.dflt_model.clone(),
            danger: self.dflt_danger,
            focus: MField::Provider,
            dd_open: false,
            dd_sel: self.dflt_provider,
            mdd_open: false,
            mdd_sel: 0,
        });
    }

    /// Model options for the modal's current provider: (id, display label).
    /// Empty when the catalog hasn't arrived or the provider has no list —
    /// the model field then stays free-text (graceful degradation).
    fn model_options(&self) -> Vec<(String, String)> {
        let Some(m) = self.modal.as_ref() else { return Vec::new() };
        let prov = self.providers.get(m.provider_idx).map(String::as_str).unwrap_or("");
        self.models
            .as_ref()
            .and_then(|c| c.providers.get(prov))
            .map(|p| {
                p.models
                    .iter()
                    .map(|e| {
                        let label = if e.label.is_empty() { e.id.clone() } else { e.label.clone() };
                        (e.id.clone(), label)
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The provider's endpoint-suggested default model id, if known.
    fn model_default(&self) -> Option<String> {
        let m = self.modal.as_ref()?;
        let prov = self.providers.get(m.provider_idx)?;
        self.models.as_ref()?.providers.get(prov)?.default.clone()
    }
}

/// Last ~200 log lines for a container, via the runtime CLI (fast, sync).
fn fetch_logs(runtime: &str, name: &str) -> Vec<String> {
    std::process::Command::new(runtime)
        .args(["logs", "--tail", "200", name])
        .output()
        .map(|o| {
            let mut s = String::from_utf8_lossy(&o.stdout).to_string();
            s.push_str(&String::from_utf8_lossy(&o.stderr));
            // Agent CLIs are themselves TUIs, so their container logs are full of
            // ANSI / cursor / alt-screen escapes. Strip them (sanitize_line also
            // collapses \r overwrites) so the log preview renders as clean text
            // instead of a garbled layout.
            s.lines()
                .map(|l| crate::ui::sanitize_line(l.trim_end()))
                .collect()
        })
        .unwrap_or_default()
}

/// Open the control room. Returns the chosen action, or None on quit.
pub fn run(
    running: Vec<RunningAgent>,
    sessions: Vec<SessionInfo>,
    providers: Vec<String>,
    init_provider: &str,
    init_model: Option<&str>,
    init_danger: bool,
    ctx: Ctx,
) -> Result<Option<Outcome>> {
    let providers = if providers.is_empty() {
        // Fallback: the registry's installed providers (data-driven, never a
        // hardcoded list).
        crate::provider_registry::ProviderRegistry::load()
            .names()
            .iter()
            .map(|s| s.to_string())
            .collect()
    } else {
        providers
    };
    let dflt_provider = providers
        .iter()
        .position(|p| p == init_provider)
        .unwrap_or(0);

    // Per-provider model-picker hints, data-driven from the provider TOMLs
    // (keyed lowercase). Shown under the model field in the new-session modal.
    let provider_hints: HashMap<String, String> = crate::provider_registry::ProviderRegistry::load()
        .all()
        .filter_map(|d| {
            d.provider
                .picker_hint
                .clone()
                .map(|h| (d.provider.name.to_lowercase(), h))
        })
        .collect();

    enable_raw_mode()?;
    // Restore the terminal on ANY exit — clean return, error, or a render panic
    // (e.g. the resize overflow in #61). Without this a panic leaves the shell in
    // raw mode + alt screen ("half in / half out"). Mirrors docker::TermGuard.
    let _guard = ControlRoomGuard;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut st = State {
        tab: 0,
        sel: [0, 0],
        tstate: [TableState::default(), TableState::default()],
        query: String::new(),
        filtering: false,
        menu_open: None,
        menu_sel: 0,
        status: default_status(),
        menu_x: Vec::new(),
        detail: None,
        confirm_kill: None,
        pin_sel: None,
        help: None,
        modal: None,
        providers,
        models: None,
        dflt_provider,
        dflt_model: init_model.unwrap_or("").to_string(),
        dflt_danger: init_danger,
        tools: None,
        config: None,
        avail_tools: Vec::new(),
        cwd_config: ctx.config_path.clone(),
        provider_hints,
    };
    let mut running = running;
    let danger = init_danger;

    let result = (|| -> Result<Option<Outcome>> {
        loop {
            // Drain background refresher data (stale-while-revalidate, v3
            // §4.5): take the newest list, keep the selection pinned to the
            // selected agent's NAME, not its row index.
            if let Some(rx) = ctx.updates.as_ref() {
                let mut newest: Option<Vec<RunningAgent>> = None;
                while let Ok(v) = rx.try_recv() {
                    newest = Some(v);
                }
                if let Some(v) = newest {
                    if st.pin_sel.is_none() {
                        let cur = filter_running(&running, &st.query);
                        st.pin_sel = cur.get(st.sel[0]).map(|&i| running[i].name.clone());
                    }
                    running = v;
                    // Live logs: keep the open detail overlay's log tail fresh.
                    if st.tab == 0 {
                        if let Some(d) = st.detail.as_mut() {
                            let cur = filter_running(&running, &st.query);
                            if let Some(&i) = cur.get(st.sel[0]) {
                                d.logs = fetch_logs(&ctx.runtime, &running[i].name);
                            }
                        }
                    }
                }
            }

            // Model catalog arriving from the background fetch.
            if let Some(rx) = ctx.models.as_ref() {
                while let Ok(cat) = rx.try_recv() {
                    st.models = Some(cat);
                }
            }

            // Image built-in tool list arriving from the background fetch. If
            // the picker is already open (opened before the list landed), refresh
            // its rows so newly-available built-ins appear without a reopen.
            if let Some(rx) = ctx.avail_tools.as_ref() {
                let mut got = false;
                while let Ok(list) = rx.try_recv() {
                    st.avail_tools = list;
                    got = true;
                }
                if got {
                    let avail = st.avail_tools.clone();
                    let installed = installed_volume_tools();
                    if let Some(t) = st.tools.as_mut() {
                        t.rows = build_tool_rows(&avail, &installed, &t.enabled);
                        t.sel = t.sel.min(t.rows.len().saturating_sub(1));
                    }
                }
            }

            // Filtered index lists for the active tab.
            let run_idx = filter_running(&running, &st.query);
            let sess_idx = filter_sessions(&sessions, &st.query);
            // Re-pin selection by agent name after a refresh reordered rows.
            if let Some(name) = st.pin_sel.take() {
                if let Some(pos) = run_idx.iter().position(|&i| running[i].name == name) {
                    st.sel[0] = pos;
                }
            }
            let len = if st.tab == 0 { run_idx.len() } else { sess_idx.len() };
            let last = len.saturating_sub(1);
            if st.sel[st.tab] > last {
                st.sel[st.tab] = last;
            }

            // Layout (also used for mouse hit-testing). Danger mode draws a
            // heavy red frame around everything (v3 §3.7), so content insets.
            // Use the LIVE terminal size, not get_frame().area(): the latter is
            // the current buffer, which is only resized inside terminal.draw().
            // On a shrink, the stale (larger) buffer area would lay everything
            // out too wide, then draw() resizes smaller and the render writes
            // past the edge → "index outside of buffer" panic (#61).
            let root = terminal
                .size()
                .map(|s| Rect::new(0, 0, s.width, s.height))
                .unwrap_or_else(|_| terminal.get_frame().area());
            let area = if danger {
                Rect::new(
                    root.x + 1,
                    root.y + 1,
                    root.width.saturating_sub(2),
                    root.height.saturating_sub(2),
                )
            } else {
                root
            };
            // Hint bars wrap on narrow terminals — their rows are computed
            // from the rendered width, never clipped.
            let cheat_h = bar_height(Bar::Top, area.width);
            let status_h = if st.filtering || !st.status.is_empty() {
                1
            } else {
                bar_height(Bar::Bot, area.width)
            };
            let chunks = Layout::vertical([
                Constraint::Length(1),        // menu bar
                Constraint::Length(cheat_h),  // cheat sheet (wraps)
                Constraint::Length(1),        // tab strip
                Constraint::Min(1),           // table
                Constraint::Length(status_h), // status / nav bar (wraps)
            ])
            .split(area);
            let (bar_r, cheat_r, tabs_r, table_r, status_r) =
                (chunks[0], chunks[1], chunks[2], chunks[3], chunks[4]);

            // Precompute menu title x-offsets for click hit-testing.
            st.menu_x.clear();
            let mut x = bar_r.x + 1;
            for (title, _) in MENUS {
                st.menu_x.push(x);
                x += title.len() as u16 + 3; // title + padding
            }

            st.tstate[st.tab].select(Some(st.sel[st.tab]));

            terminal.draw(|f| {
                if danger {
                    f.render_widget(
                        Block::default()
                            .borders(Borders::ALL)
                            .border_style(crate::theme::danger_border()),
                        root,
                    );
                }
                draw_bar(f, bar_r, &st, danger);
                f.render_widget(
                    Paragraph::new(bar_line(Bar::Top))
                        .wrap(ratatui::widgets::Wrap { trim: false })
                        .style(Style::default().bg(Color::Indexed(235))),
                    cheat_r,
                );
                draw_tabs(f, tabs_r, &st, run_idx.len(), sess_idx.len());
                if st.tab == 0 {
                    draw_running(f, table_r, &running, &run_idx, &mut st.tstate[0]);
                } else {
                    draw_sessions(f, table_r, &sessions, &sess_idx, &mut st.tstate[1]);
                }
                draw_status(f, status_r, &st);
                if st.detail.is_some() {
                    draw_detail(f, area, &st, &ctx, &running, &run_idx, &sessions, &sess_idx);
                }
                if let Some(h) = st.help {
                    draw_help(f, area, h);
                }
                if st.modal.is_some() {
                    draw_modal(f, area, &st);
                }
                if st.tools.is_some() {
                    draw_tools(f, area, &st);
                }
                if st.config.is_some() {
                    draw_config(f, area, &st);
                }
                if st.confirm_kill.is_some() {
                    draw_confirm(f, area, &st);
                }
                if let Some(mi) = st.menu_open {
                    draw_dropdown(f, bar_r, &st, mi);
                }
            })?;

            // Poll with a timeout so refresher data shows without a keypress.
            if !event::poll(std::time::Duration::from_millis(250))? {
                continue;
            }
            match event::read()? {
                Event::Key(k) if k.kind == KeyEventKind::Press => {
                    if let Some(action) = on_key(&mut st, k.code, k.modifiers, &ctx, &running, &run_idx, &sessions, &sess_idx, last) {
                        match action {
                            Flow::Return(a) => return Ok(a),
                            Flow::Continue => {}
                        }
                    }
                }
                Event::Mouse(m) => {
                    if let Some(action) = on_mouse(&mut st, m, area, bar_r, tabs_r, table_r, &ctx, &running, &run_idx, last) {
                        match action {
                            Flow::Return(a) => return Ok(a),
                            Flow::Continue => {}
                        }
                    }
                }
                _ => {}
            }
        }
    })();

    disable_raw_mode().ok();
    execute!(terminal.backend_mut(), LeaveAlternateScreen, DisableMouseCapture).ok();
    terminal.show_cursor().ok();
    result
}

/// Restores the terminal (cooked mode, main screen, mouse off, cursor shown) on
/// drop, so a panic in the control-room render can't strand the shell "half in /
/// half out". Created right after `enable_raw_mode()`; the explicit teardown on
/// the clean path runs first and double-restoring is harmless.
struct ControlRoomGuard;

impl Drop for ControlRoomGuard {
    fn drop(&mut self) {
        disable_raw_mode().ok();
        execute!(
            io::stdout(),
            LeaveAlternateScreen,
            DisableMouseCapture,
            crossterm::cursor::Show
        )
        .ok();
    }
}

enum Flow {
    Return(Option<Outcome>),
    Continue,
}

fn default_status() -> String {
    String::new()
}

// ── filtering ───────────────────────────────────────────────────────────────

fn filter_running(running: &[RunningAgent], q: &str) -> Vec<usize> {
    let ql = q.to_lowercase();
    let mut idx: Vec<usize> = running
        .iter()
        .enumerate()
        .filter(|(_, r)| {
            q.is_empty()
                || r.name.to_lowercase().contains(&ql)
                || r.provider.to_lowercase().contains(&ql)
                || r.last_log.to_lowercase().contains(&ql)
        })
        .map(|(i, _)| i)
        .collect();
    // Blocked agents first (needs-input → working → …), stable within rank
    // so docker's ordering is preserved inside each group (v3 §1.3).
    idx.sort_by_key(|&i| running[i].state.rank());
    idx
}

fn filter_sessions(sessions: &[SessionInfo], q: &str) -> Vec<usize> {
    let ql = q.to_lowercase();
    sessions
        .iter()
        .enumerate()
        .filter(|(_, s)| {
            q.is_empty()
                || s.id.to_lowercase().contains(&ql)
                || s.provider.as_deref().unwrap_or("").to_lowercase().contains(&ql)
                || s.workspace.as_deref().unwrap_or("").to_lowercase().contains(&ql)
        })
        .map(|(i, _)| i)
        .collect()
}

// ── rendering ─────────────────────────────────────────────────────────────

fn draw_bar(f: &mut ratatui::Frame, r: Rect, st: &State, danger: bool) {
    let mut spans = vec![Span::raw(" ")];
    for (i, (title, _)) in MENUS.iter().enumerate() {
        let active = st.menu_open == Some(i);
        let style = if active {
            Style::default().bg(Color::Indexed(238)).fg(Color::White).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Cyan)
        };
        spans.push(Span::styled(format!(" {title} "), style));
        spans.push(Span::raw(" "));
    }
    f.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(Color::Indexed(236))),
        r,
    );
    // Right side: the DANGER badge when armed (v3 §3.7), else the app label.
    let label = if danger { " ⚠ DANGER " } else { " n8 control room " };
    let style = if danger {
        crate::theme::danger_badge()
    } else {
        Style::default().fg(Color::DarkGray)
    };
    if r.width as usize > label.chars().count() {
        let lx = r.x + r.width - label.chars().count() as u16 - 1;
        f.render_widget(
            Paragraph::new(Span::styled(label, style)),
            Rect::new(lx, r.y, label.chars().count() as u16, 1),
        );
    }
}

fn draw_tabs(f: &mut ratatui::Frame, r: Rect, st: &State, n_run: usize, n_sess: usize) {
    let titles = vec![
        Line::from(format!(" Running ({n_run}) ")),
        Line::from(format!(" Sessions ({n_sess}) ")),
    ];
    let tabs = Tabs::new(titles)
        .select(st.tab)
        .highlight_style(Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD))
        .divider("");
    f.render_widget(tabs, r);
}

fn draw_running(
    f: &mut ratatui::Frame,
    r: Rect,
    running: &[RunningAgent],
    idx: &[usize],
    state: &mut TableState,
) {
    let header = Row::new(["ST", "NAME", "PROV", "SESSION ID", "UPTIME", "WORKSPACE"])
        .style(Style::default().fg(Color::Indexed(244)).add_modifier(Modifier::BOLD));
    let rows: Vec<Row> = idx
        .iter()
        .map(|&i| {
            let a = &running[i];
            let sid: String = a
                .session_id
                .as_deref()
                .map(|s| s.chars().take(13).collect())
                .unwrap_or_else(|| "—".into());
            Row::new([
                Cell::from(a.state.glyph()).style(a.state.style()),
                Cell::from(a.name.clone()).style(Style::default().fg(Color::Cyan)),
                Cell::from(a.provider.chars().take(12).collect::<String>())
                    .style(Style::default().fg(Color::Green)),
                Cell::from(sid),
                Cell::from(a.uptime.clone()).style(Style::default().fg(Color::Gray)),
                Cell::from(a.workspace.clone().unwrap_or_else(|| "—".into()))
                    .style(Style::default().fg(Color::DarkGray)),
            ])
        })
        .collect();
    let widths = [
        Constraint::Length(2),
        Constraint::Length(16),
        Constraint::Length(12),
        Constraint::Length(14),
        Constraint::Length(12),
        Constraint::Min(10),
    ];
    render_table(f, r, header, rows, idx.len(), widths, state, "Running — ⏎ detail · a attach · k kill · l logs");
}

fn draw_sessions(
    f: &mut ratatui::Frame,
    r: Rect,
    sessions: &[SessionInfo],
    idx: &[usize],
    state: &mut TableState,
) {
    let header = Row::new(["SESSION ID", "PROV", "MODIFIED", "WORKSPACE"])
        .style(Style::default().fg(Color::Indexed(244)).add_modifier(Modifier::BOLD));
    let rows: Vec<Row> = idx
        .iter()
        .map(|&i| {
            let s = &sessions[i];
            let id: String = s.id.chars().take(13).collect();
            let modified: String = s.modified.as_deref().unwrap_or("").chars().take(16).collect();
            Row::new([
                Cell::from(id).style(Style::default().fg(Color::Cyan)),
                Cell::from(s.provider.clone().unwrap_or_else(|| "-".into()))
                    .style(Style::default().fg(Color::Green)),
                Cell::from(modified).style(Style::default().fg(Color::Gray)),
                Cell::from(s.workspace.clone().unwrap_or_default())
                    .style(Style::default().fg(Color::DarkGray)),
            ])
        })
        .collect();
    let widths = [
        Constraint::Length(14),
        Constraint::Length(10),
        Constraint::Length(18),
        Constraint::Min(10),
    ];
    render_table(f, r, header, rows, idx.len(), widths, state, "Sessions — ⏎ resume (Ctrl+⏎/. = here)");
}

#[allow(clippy::too_many_arguments)]
fn render_table(
    f: &mut ratatui::Frame,
    r: Rect,
    header: Row,
    rows: Vec<Row>,
    total: usize,
    widths: impl IntoIterator<Item = Constraint>,
    state: &mut TableState,
    title: &str,
) {
    let table = Table::new(rows, widths)
        .header(header)
        .block(Block::default().borders(Borders::ALL).title(format!("  {title}  ")))
        .row_highlight_style(
            Style::default().bg(Color::Indexed(238)).fg(Color::White).add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");
    f.render_stateful_widget(table, r, state);

    // Scrollbar reflecting offset/total.
    if total > r.height.saturating_sub(3) as usize {
        let mut sb = ScrollbarState::new(total).position(state.offset());
        f.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight).begin_symbol(None).end_symbol(None),
            r,
            &mut sb,
        );
    }
}

fn draw_status(f: &mut ratatui::Frame, r: Rect, st: &State) {
    // Bottom bar = navigation only, colorized. While filtering, show the input;
    // a transient `status` message (if any) takes priority.
    let line = if st.filtering {
        Line::from(vec![
            Span::styled(" find: ", Style::default().fg(Color::Yellow)),
            Span::styled(format!("{}▏", st.query), Style::default().fg(Color::White)),
            Span::styled("   esc clears", Style::default().fg(Color::DarkGray)),
        ])
    } else if !st.status.is_empty() {
        Line::from(Span::styled(format!(" {}", st.status), Style::default().fg(Color::Gray)))
    } else {
        bar_line(Bar::Bot)
    };
    f.render_widget(
        Paragraph::new(line)
            .wrap(ratatui::widgets::Wrap { trim: false })
            .style(Style::default().bg(Color::Indexed(236))),
        r,
    );
}

fn draw_dropdown(f: &mut ratatui::Frame, bar: Rect, st: &State, mi: usize) {
    let items = MENUS[mi].1;
    let x = *st.menu_x.get(mi).unwrap_or(&bar.x);
    let w = items.iter().map(|s| s.len()).max().unwrap_or(8) as u16 + 4;
    let h = items.len() as u16 + 2;
    let dr = Rect::new(x, bar.y + 1, w.min(bar.width.saturating_sub(x - bar.x)), h);
    let lines: Vec<Line> = items
        .iter()
        .enumerate()
        .map(|(i, it)| {
            let sel = i == st.menu_sel;
            let style = if sel {
                Style::default().bg(Color::Indexed(238)).fg(Color::White).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Gray)
            };
            Line::from(Span::styled(format!(" {it} "), style))
        })
        .collect();
    f.render_widget(Clear, dr);
    f.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL)),
        dr,
    );
}

fn kv(key: &str, val: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{key:>11}: "), Style::default().fg(Color::Indexed(244))),
        Span::styled(val.to_string(), Style::default().fg(Color::White)),
    ])
}

fn centered(parent: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(parent.width);
    let h = h.min(parent.height);
    Rect::new(
        parent.x + parent.width.saturating_sub(w) / 2,
        parent.y + parent.height.saturating_sub(h) / 2,
        w,
        h,
    )
}

/// Rect for the v2 detail overlay: ~85% of the frame (v3 §3.4 — not the old
/// cramped box). Also used by mouse hit-testing.
fn detail_rect(area: Rect) -> Rect {
    centered(
        area,
        (area.width as u32 * 85 / 100) as u16,
        (area.height as u32 * 85 / 100) as u16,
    )
}

/// Detail overlay (v3 §3.4). Running agents get the sectioned Meta/Tools/Logs
/// view with a scrollable, tail-following log pane; sessions keep the compact
/// metadata card.
#[allow(clippy::too_many_arguments)]
fn draw_detail(
    f: &mut ratatui::Frame,
    area: Rect,
    st: &State,
    ctx: &Ctx,
    running: &[RunningAgent],
    run_idx: &[usize],
    sessions: &[SessionInfo],
    sess_idx: &[usize],
) {
    let Some(d) = st.detail.as_ref() else { return };
    let yellow = Style::default().fg(Color::Yellow);

    if st.tab == 1 {
        // Sessions: compact card, unchanged semantics.
        let lines: Vec<Line> = match sess_idx.get(st.sel[1]).map(|&i| &sessions[i]) {
            Some(s) => vec![
                kv("session id", &s.id),
                kv("provider", s.provider.as_deref().unwrap_or("-")),
                kv("modified", s.modified.as_deref().unwrap_or("")),
                kv("workspace", s.workspace.as_deref().unwrap_or("—")),
                kv("path", &s.path),
                Line::from(""),
                Line::from(Span::styled("⏎/a resume · . resume here · esc back", yellow)),
            ],
            None => vec![Line::from("no selection")],
        };
        let h = (lines.len() as u16 + 2).min(area.height);
        let dr = centered(area, 84.min(area.width.saturating_sub(2)), h);
        f.render_widget(Clear, dr);
        f.render_widget(
            Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title("  detail  ")),
            dr,
        );
        return;
    }

    let Some(a) = run_idx.get(st.sel[0]).map(|&i| &running[i]) else {
        return;
    };
    let dr = detail_rect(area);
    f.render_widget(Clear, dr);

    // Section tabs in the title: [Meta Tools Logs], current highlighted.
    let mut title_spans = vec![Span::raw(format!("  {} · {} ", a.name, a.provider))];
    for (i, name) in ["Meta", "Tools", "Logs"].iter().enumerate() {
        title_spans.push(Span::raw(" "));
        title_spans.push(Span::styled(
            format!(" {name} "),
            if d.section == i as u8 {
                Style::default().bg(Color::Cyan).fg(Color::Black).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Gray)
            },
        ));
    }
    title_spans.push(Span::raw(" "));
    f.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan))
            .title(Line::from(title_spans)),
        dr,
    );

    let inner = Rect::new(
        dr.x + 2,
        dr.y + 1,
        dr.width.saturating_sub(4),
        dr.height.saturating_sub(2),
    );
    // Header: state + attach hint + uptime, always visible above the section.
    let header = Line::from(vec![
        Span::styled(a.state.glyph(), a.state.style()),
        Span::styled(format!(" {} ", a.state.label()), a.state.style()),
        Span::styled(format!("· uptime {} ", a.uptime), Style::default().fg(Color::Gray)),
        Span::styled(
            format!("· {}", a.workspace.as_deref().unwrap_or("—")),
            Style::default().fg(Color::DarkGray),
        ),
    ]);
    f.render_widget(Paragraph::new(header), Rect::new(inner.x, inner.y, inner.width, 1));

    let body = Rect::new(
        inner.x,
        inner.y + 2,
        inner.width,
        inner.height.saturating_sub(3),
    );
    match d.section {
        0 => {
            let lines = vec![
                kv("name", &a.name),
                kv("provider", &a.provider),
                kv("state", a.state.label()),
                kv("session id", a.session_id.as_deref().unwrap_or("—")),
                kv("uptime", &a.uptime),
                kv("workspace", a.workspace.as_deref().unwrap_or("—")),
            ];
            f.render_widget(Paragraph::new(lines), body);
        }
        1 => {
            let mut lines: Vec<Line> = vec![Line::from(Span::styled(
                "active MCP tools (config):",
                Style::default().add_modifier(Modifier::BOLD),
            ))];
            if ctx.tools.is_empty() {
                lines.push(Line::from(Span::styled(
                    "  (none configured)",
                    Style::default().fg(Color::DarkGray),
                )));
            } else {
                for t in &ctx.tools {
                    lines.push(Line::from(format!("  · {t}")));
                }
            }
            f.render_widget(Paragraph::new(lines), body);
        }
        _ => {
            // Logs: window of the tail, scrolled `d.scroll` lines up from the
            // end. scroll == 0 → following the tail.
            let h = body.height as usize;
            let total = d.logs.len();
            let end = total.saturating_sub(d.scroll.min(total));
            let start = end.saturating_sub(h);
            let lines: Vec<Line> = d.logs[start..end]
                .iter()
                .map(|l| Line::from(l.clone()))
                .collect();
            f.render_widget(Paragraph::new(lines), body);
            if total > h {
                let mut sb = ScrollbarState::new(total.saturating_sub(h)).position(start);
                f.render_stateful_widget(
                    Scrollbar::new(ScrollbarOrientation::VerticalRight)
                        .begin_symbol(None)
                        .end_symbol(None),
                    body,
                    &mut sb,
                );
            }
            if d.scroll > 0 {
                let hint = " ↓ End re-follows tail ";
                f.render_widget(
                    Paragraph::new(Span::styled(hint, Style::default().fg(Color::Yellow))),
                    Rect::new(
                        body.x + body.width.saturating_sub(hint.len() as u16 + 1),
                        body.y + body.height.saturating_sub(1),
                        hint.len() as u16,
                        1,
                    ),
                );
            }
        }
    }

    // Footer: the overlay's own keymap (sanctioned exception — focus layer).
    let footer = Line::from(Span::styled(
        " m meta · t tools · l logs · ↑↓ scroll · a attach · k kill · esc close ",
        yellow,
    ));
    f.render_widget(
        Paragraph::new(footer),
        Rect::new(inner.x, dr.y + dr.height.saturating_sub(2), inner.width, 1),
    );
}

/// Kill-confirm modal rects: (frame, kill button, cancel button).
fn confirm_rects(area: Rect) -> (Rect, Rect, Rect) {
    let fr = centered(area, 44.min(area.width.saturating_sub(2)), 7);
    let bx = fr.x + 3;
    let by = fr.y + 5;
    (fr, Rect::new(bx, by, 10, 1), Rect::new(bx + 14, by, 14, 1))
}

/// Kill confirmation (v3 §3.8): friction on the destructive verb, and the
/// consequence is spelled out (the session survives and stays resumable).
fn draw_confirm(f: &mut ratatui::Frame, area: Rect, st: &State) {
    let Some(name) = st.confirm_kill.as_ref() else { return };
    let (fr, kb, cb) = confirm_rects(area);
    f.render_widget(Clear, fr);
    f.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Red))
            .title("  Kill agent?  "),
        fr,
    );
    let lines = vec![
        Line::from(Span::styled(
            format!(" {name} "),
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            " The session is preserved and resumable. ",
            Style::default().fg(Color::Gray),
        )),
    ];
    f.render_widget(
        Paragraph::new(lines),
        Rect::new(fr.x + 2, fr.y + 2, fr.width.saturating_sub(4), 2),
    );
    f.render_widget(
        Paragraph::new(Span::styled(
            " Kill ⏎ ",
            Style::default().bg(Color::Red).fg(Color::White).add_modifier(Modifier::BOLD),
        )),
        kb,
    );
    f.render_widget(
        Paragraph::new(Span::styled(" Cancel Esc ", Style::default().fg(Color::Yellow))),
        cb,
    );
}

/// Help overlay — Keys (rendered from the KEYS registry) or About.
fn draw_help(f: &mut ratatui::Frame, area: Rect, kind: u8) {
    let lines: Vec<Line> = if kind == 1 {
        let mut v = vec![Line::from(Span::styled(
            "Keys",
            Style::default().add_modifier(Modifier::BOLD),
        ))];
        for (k, what, _) in KEYS {
            v.push(Line::from(vec![
                Span::styled(format!("{k:>10}  "), Style::default().fg(Color::Yellow)),
                Span::raw(*what),
            ]));
        }
        v.push(Line::from(""));
        v.push(Line::from(Span::styled("esc/q close", Style::default().fg(Color::Gray))));
        v
    } else {
        vec![
            Line::from(Span::styled(
                "nemesis8 control room",
                Style::default().add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from("Run AI agents in Docker. Bare `n8` opens this."),
            Line::from(format!("version {}", env!("CARGO_PKG_VERSION"))),
            Line::from(""),
            Line::from(Span::styled("esc/q close", Style::default().fg(Color::Gray))),
        ]
    };
    let w = 52.min(area.width.saturating_sub(2));
    let h = (lines.len() as u16 + 2).min(area.height.saturating_sub(1));
    let dr = centered(area, w, h);
    f.render_widget(Clear, dr);
    f.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title("  Help  ")),
        dr,
    );
}

// ── new-session modal ────────────────────────────────────────────────────────

fn next_field(f: MField) -> MField {
    match f {
        MField::Provider => MField::Model,
        MField::Model => MField::Danger,
        MField::Danger => MField::Launch,
        MField::Launch => MField::Cancel,
        MField::Cancel => MField::Provider,
    }
}
fn prev_field(f: MField) -> MField {
    match f {
        MField::Provider => MField::Cancel,
        MField::Model => MField::Provider,
        MField::Danger => MField::Model,
        MField::Launch => MField::Danger,
        MField::Cancel => MField::Launch,
    }
}

/// Take the modal's choices and return the launch outcome.
fn confirm_modal(st: &mut State) -> Flow {
    if let Some(m) = st.modal.take() {
        let provider = st
            .providers
            .get(m.provider_idx)
            .or(st.providers.first())
            .cloned()
            .unwrap_or_default();
        let t = m.model.trim();
        let model = if t.is_empty() { None } else { Some(t.to_string()) };
        return Flow::Return(Some(Outcome::NewSession {
            provider,
            model,
            danger: m.danger,
        }));
    }
    Flow::Continue
}

fn hit(r: Rect, col: u16, row: u16) -> bool {
    col >= r.x && col < r.x + r.width && row >= r.y && row < r.y + r.height
}
fn hit_col(r: Rect, col: u16) -> bool {
    col >= r.x && col < r.x + r.width
}

/// Modal layout rects: (modal, provider, model, danger, launch, cancel).
fn modal_rects(area: Rect) -> (Rect, Rect, Rect, Rect, Rect, Rect) {
    let modal = centered(
        area,
        58.min(area.width.saturating_sub(2)),
        // 11, not 9: two extra rows for the per-provider model-picker hint.
        11.min(area.height.saturating_sub(2)),
    );
    let ix = modal.x + 2;
    let iw = modal.width.saturating_sub(4);
    (
        modal,
        Rect::new(ix, modal.y + 2, iw, 1),
        Rect::new(ix, modal.y + 3, iw, 1),
        Rect::new(ix, modal.y + 4, iw, 1),
        Rect::new(ix, modal.y + 6, 10, 1),
        Rect::new(ix + 12, modal.y + 6, 10, 1),
    )
}

fn draw_modal(f: &mut ratatui::Frame, area: Rect, st: &State) {
    let Some(m) = st.modal.as_ref() else { return };
    let (modal, pr, mr, dr, lb, cb) = modal_rects(area);
    f.render_widget(Clear, modal);
    f.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan))
            .title("  New session  "),
        modal,
    );
    let fld = |label: &str, value: String, focused: bool| -> Line<'static> {
        Line::from(vec![
            Span::styled(format!("{label:<9} "), Style::default().fg(Color::Indexed(244))),
            Span::styled(
                value,
                if focused {
                    Style::default().fg(Color::White).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::Gray)
                },
            ),
        ])
    };
    let prov = st.providers.get(m.provider_idx).cloned().unwrap_or_default();
    f.render_widget(
        Paragraph::new(fld("Provider", format!("[ {prov}  ▾ ]"), m.focus == MField::Provider)),
        pr,
    );
    let has_models = !st.model_options().is_empty();
    let modelval = match (m.model.is_empty(), has_models) {
        (true, true) => match st.model_default() {
            Some(d) => format!("[ default ({d})  ▾ ]"),
            None => "[ default  ▾ ]".to_string(),
        },
        (true, false) => "[ default ]".to_string(),
        (false, true) => format!("[ {}  ▾ ]", m.model),
        (false, false) => format!("[ {}▏ ]", m.model),
    };
    f.render_widget(Paragraph::new(fld("Model", modelval, m.focus == MField::Model)), mr);
    f.render_widget(
        Paragraph::new(fld(
            "Danger",
            format!("[{}] skip approvals + sandbox", if m.danger { "x" } else { " " }),
            m.focus == MField::Danger,
        )),
        dr,
    );
    let btn = |label: &str, focused: bool| -> Paragraph<'static> {
        Paragraph::new(Span::styled(
            format!(" {label} "),
            if focused {
                Style::default().bg(Color::Cyan).fg(Color::Black).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Yellow)
            },
        ))
    };
    f.render_widget(btn("Launch", m.focus == MField::Launch), lb);
    f.render_widget(btn("Cancel", m.focus == MField::Cancel), cb);

    // Per-provider model-picker hint, fed from the provider TOML's picker_hint.
    if let Some(hint) = st.provider_hints.get(&prov.to_lowercase()) {
        let hr = Rect::new(modal.x + 2, modal.y + 8, modal.width.saturating_sub(4), 2);
        f.render_widget(
            Paragraph::new(hint.as_str())
                .style(
                    Style::default()
                        .fg(Color::Indexed(244))
                        .add_modifier(Modifier::ITALIC),
                )
                .wrap(ratatui::widgets::Wrap { trim: true }),
            hr,
        );
    }

    // Provider pulldown (rendered last so it sits above the model row).
    if m.dd_open {
        let np = st.providers.len() as u16;
        let h = (np + 2).min(area.height.saturating_sub(pr.y + 1));
        let dd = Rect::new(pr.x, pr.y + 1, pr.width.clamp(12, 30), h);
        f.render_widget(Clear, dd);
        let lines: Vec<Line> = st
            .providers
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let sel = i == m.dd_sel;
                Line::from(Span::styled(
                    format!(" {p} "),
                    if sel {
                        Style::default().bg(Color::Indexed(238)).fg(Color::White)
                    } else {
                        Style::default().fg(Color::Gray)
                    },
                ))
            })
            .collect();
        f.render_widget(
            Paragraph::new(lines).block(Block::default().borders(Borders::ALL)),
            dd,
        );
    }

    // Model pulldown: row 0 = "default" (no --model), then the catalog list.
    if m.mdd_open {
        let opts = st.model_options();
        let rows = opts.len() + 1;
        let h = ((rows as u16) + 2).min(area.height.saturating_sub(mr.y + 1)).max(3);
        let dd = Rect::new(mr.x, mr.y + 1, mr.width.clamp(20, 44), h);
        f.render_widget(Clear, dd);
        let default_label = match st.model_default() {
            Some(d) => format!(" default ({d}) "),
            None => " default ".to_string(),
        };
        let mut lines: Vec<Line> = vec![Line::from(Span::styled(
            default_label,
            if m.mdd_sel == 0 {
                Style::default().bg(Color::Indexed(238)).fg(Color::White)
            } else {
                Style::default().fg(Color::Gray)
            },
        ))];
        for (i, (_, label)) in opts.iter().enumerate() {
            let sel = m.mdd_sel == i + 1;
            lines.push(Line::from(Span::styled(
                format!(" {label} "),
                if sel {
                    Style::default().bg(Color::Indexed(238)).fg(Color::White)
                } else {
                    Style::default().fg(Color::Gray)
                },
            )));
        }
        f.render_widget(
            Paragraph::new(lines)
                .scroll((m.mdd_sel.saturating_sub(h as usize - 3) as u16, 0))
                .block(Block::default().borders(Borders::ALL)),
            dd,
        );
    }
}

// ── tools picker ─────────────────────────────────────────────────────────────

/// Short label for a `.nemesis8.toml` path: its workspace directory name.
fn workspace_label(config_path: &Path) -> String {
    let dir = config_path.parent().unwrap_or(config_path);
    dir.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| dir.to_string_lossy().into_owned())
}

/// Build the picker's row list: an always-on binary header, the image built-ins
/// (sorted), then any configured entries that aren't built-ins (URLs / host-only),
/// and finally any `.py` sitting in the volume's `mcp/` drawer that the image no
/// longer ships (`Stale` orphans — the ghost source), so all of it is visible
/// and removable. `installed` is the volume's `mcp/*.py` filenames.
fn build_tool_rows(
    avail: &[String],
    installed: &[String],
    enabled: &HashSet<String>,
) -> Vec<(String, ToolKind)> {
    let mut rows: Vec<(String, ToolKind)> = Vec::new();
    // Registry socket/stdio MCP servers (blender, hyperia, launcher-added …) at
    // the TOP — the headline tools you actually toggle. Embedded + the
    // container-mapped user dir; toggling adds/removes the NAME in mcp_tools.
    let registry = crate::mcp_registry::McpRegistry::load_host(&crate::paths::data_home());
    let mut reg_names: Vec<String> = registry.names().iter().map(|s| s.to_string()).collect();
    reg_names.sort();
    let reg_set: HashSet<&str> = reg_names.iter().map(String::as_str).collect();
    for n in &reg_names {
        rows.push((n.clone(), ToolKind::Registry));
    }
    // Always-on binary header, then the image .py built-ins (sorted).
    rows.push(("nuts-files".to_string(), ToolKind::Binary));
    let mut builtins: Vec<String> = avail.to_vec();
    builtins.sort();
    builtins.dedup();
    let builtin_set: HashSet<&str> = builtins.iter().map(String::as_str).collect();
    for b in &builtins {
        rows.push((b.clone(), ToolKind::Builtin));
    }
    let mut extras: Vec<&String> = enabled
        .iter()
        .filter(|t| {
            t.as_str() != "nuts-files"
                && !builtin_set.contains(t.as_str())
                && !reg_set.contains(t.as_str())
        })
        .collect();
    extras.sort();
    let extra_set: HashSet<&str> = extras.iter().map(|s| s.as_str()).collect();
    for e in &extras {
        let kind = if e.starts_with("http://") || e.starts_with("https://") {
            ToolKind::Url
        } else {
            ToolKind::Extra
        };
        rows.push(((*e).clone(), kind));
    }
    // Volume orphans: present on disk, not shipped by the image, not already a
    // row. These are the junk-drawer stragglers that become ghost servers.
    let mut stale: Vec<&String> = installed
        .iter()
        .filter(|f| !builtin_set.contains(f.as_str()) && !extra_set.contains(f.as_str()))
        .collect();
    stale.sort();
    stale.dedup();
    for s in stale {
        rows.push((s.clone(), ToolKind::Stale));
    }
    rows
}

/// The volume's installed MCP tools — `~/.nemesis8/home/mcp/*.py` filenames.
/// Read straight off the host (the drawer is a bind-mounted host dir), so the
/// picker can show and delete orphans without spinning a container.
fn installed_volume_tools() -> Vec<String> {
    let dir = crate::paths::data_home().join("mcp");
    std::fs::read_dir(&dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter(|e| e.path().extension().is_some_and(|x| x == "py"))
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default()
}

/// Indices into `rows` matching the current substring filter (all when empty).
fn filter_tool_rows(t: &ToolsModal) -> Vec<usize> {
    if t.filter.is_empty() {
        return (0..t.rows.len()).collect();
    }
    let q = t.filter.to_lowercase();
    t.rows
        .iter()
        .enumerate()
        .filter(|(_, (n, _))| n.to_lowercase().contains(&q))
        .map(|(i, _)| i)
        .collect()
}

/// Open the tools picker editing `target`'s `.nemesis8.toml`.
fn open_tools_for(st: &mut State, target: PathBuf) {
    let target_label = workspace_label(&target);
    let enabled: HashSet<String> = crate::config::read_mcp_tools(&target).into_iter().collect();
    let installed = installed_volume_tools();
    let rows = build_tool_rows(&st.avail_tools, &installed, &enabled);
    st.tools = Some(ToolsModal {
        target,
        target_label,
        rows,
        enabled,
        sel: 0,
        filter: String::new(),
        filtering: false,
        status: String::new(),
        confirm_delete: None,
        adding: None,
    });
}

/// Which `.nemesis8.toml` the picker edits: the highlighted session's workspace
/// when resuming (Sessions tab), otherwise the cwd (what a New session uses).
fn tools_target(st: &State, sessions: &[SessionInfo], sess_idx: &[usize]) -> PathBuf {
    if st.tab == 1 {
        if let Some(ws) = sess_idx
            .get(st.sel[1])
            .and_then(|&i| sessions.get(i))
            .and_then(|s| s.workspace.as_deref())
        {
            return Path::new(ws).join(".nemesis8.toml");
        }
    }
    st.cwd_config.clone()
}

/// Toggle the highlighted tool and persist the new set to the target config.
fn toggle_tool(st: &mut State) {
    let Some(t) = st.tools.as_mut() else { return };
    let filtered = filter_tool_rows(t);
    let Some(&ri) = filtered.get(t.sel) else { return };
    let (name, kind) = t.rows[ri].clone();
    if kind == ToolKind::Binary {
        t.status = "nuts-files is built in — always on".to_string();
        return;
    }
    if !t.enabled.remove(&name) {
        t.enabled.insert(name.clone());
    }
    let mut list: Vec<String> = t.enabled.iter().cloned().collect();
    list.sort();
    match crate::config::write_mcp_tools(&t.target, &list) {
        Ok(()) => {
            let verb = if t.enabled.contains(&name) { "added" } else { "removed" };
            t.status = format!("{verb} {name} → {}", t.target_label);
        }
        Err(e) => t.status = format!("save failed: {e}"),
    }
}

/// Validate the add-server form, write the registry TOML to the container-mapped
/// user dir (`<data_home>/.nemesis8/mcp/<name>.toml`), enable it in the target
/// workspace, refresh the rows, and close the form. Errors stay in the form.
fn submit_add_server(st: &mut State) {
    let (name, url, token_env) = {
        let a = st.tools.as_ref().unwrap().adding.as_ref().unwrap();
        (
            a.name.trim().to_string(),
            a.url.trim().to_string(),
            a.token_env.trim().to_string(),
        )
    };
    let set_err = |st: &mut State, msg: String| {
        st.tools.as_mut().unwrap().adding.as_mut().unwrap().error = msg;
    };

    let name_ok = !name.is_empty()
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if !name_ok {
        set_err(st, "name: non-empty, letters/digits/-/_ only".to_string());
        return;
    }
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        set_err(st, "url must start with http:// or https://".to_string());
        return;
    }

    let mut toml = String::from("# Added via the n8 launcher (tools picker → a).\n[server]\n");
    toml.push_str(&format!("name = \"{name}\"\n"));
    toml.push_str(&format!("url = \"{url}\"\n"));
    toml.push_str("transport = \"auto\"\n");
    if !token_env.is_empty() {
        toml.push_str(&format!("bearer_token_env = \"{token_env}\"\n"));
    }

    let dir = crate::mcp_registry::host_user_mcp_dir(&crate::paths::data_home());
    if let Err(e) = std::fs::create_dir_all(&dir) {
        set_err(st, format!("mkdir failed: {e}"));
        return;
    }
    if let Err(e) = std::fs::write(dir.join(format!("{name}.toml")), toml) {
        set_err(st, format!("write failed: {e}"));
        return;
    }

    // Enable it in the target workspace's mcp_tools.
    let save = {
        let t = st.tools.as_mut().unwrap();
        t.enabled.insert(name.clone());
        let mut list: Vec<String> = t.enabled.iter().cloned().collect();
        list.sort();
        crate::config::write_mcp_tools(&t.target, &list)
    };
    if let Err(e) = save {
        set_err(st, format!("enable failed: {e}"));
        return;
    }

    // Refresh rows so the new server shows, then close the form.
    let avail = st.avail_tools.clone();
    let installed = installed_volume_tools();
    let t = st.tools.as_mut().unwrap();
    t.rows = build_tool_rows(&avail, &installed, &t.enabled);
    t.adding = None;
    t.status = format!("added server {name} → {}", t.target_label);
}

/// The currently-highlighted (name, kind), respecting the filter. None if empty.
fn current_tool(t: &ToolsModal) -> Option<(String, ToolKind)> {
    let filtered = filter_tool_rows(t);
    filtered.get(t.sel).map(|&ri| t.rows[ri].clone())
}

/// `d`: arm a delete confirmation for the highlighted tool (or, if already armed
/// for the same tool, perform the delete). Binary/URL rows have no file to
/// delete — toggling off (space) is how you drop those.
fn request_delete(st: &mut State) {
    // Decide under a short immutable borrow so we can re-borrow mutably below.
    enum Act {
        Reject(String),
        Arm(String, String),
        Delete,
    }
    let act = {
        let Some(t) = st.tools.as_ref() else { return };
        let Some((name, kind)) = current_tool(t) else { return };
        match kind {
            ToolKind::Binary => Act::Reject("nuts-files is built in — can't delete".to_string()),
            ToolKind::Url => {
                Act::Reject(format!("{name} is a URL — press space to unregister it"))
            }
            ToolKind::Registry => Act::Reject(format!(
                "{name} is a registry server — space to enable/disable (delete its TOML to remove)"
            )),
            _ if t.confirm_delete.as_deref() == Some(name.as_str()) => Act::Delete,
            _ => {
                let extra = if kind == ToolKind::Builtin {
                    " (image-shipped — reinstalls next launch)"
                } else {
                    ""
                };
                Act::Arm(
                    name.clone(),
                    format!("delete {name} from disk?{extra}  d again / esc to cancel"),
                )
            }
        }
    };
    match act {
        Act::Delete => delete_tool(st),
        Act::Reject(msg) => {
            if let Some(t) = st.tools.as_mut() {
                t.status = msg;
            }
        }
        Act::Arm(name, msg) => {
            if let Some(t) = st.tools.as_mut() {
                t.confirm_delete = Some(name);
                t.status = msg;
            }
        }
    }
}

/// Delete the highlighted tool's `.py` from the volume drawer, drop its
/// antigravity schema-cache dir, and unregister it from the target config.
/// This is the junk-drawer purge — the file is gone for all workspaces.
fn delete_tool(st: &mut State) {
    let avail = st.avail_tools.clone();
    let Some(t) = st.tools.as_mut() else { return };
    let Some((name, _)) = current_tool(t) else { return };
    let home = crate::paths::data_home();
    let mut removed = false;

    let file = home.join("mcp").join(&name);
    if file.is_file() {
        match std::fs::remove_file(&file) {
            Ok(()) => removed = true,
            Err(e) => {
                t.status = format!("delete failed: {e}");
                t.confirm_delete = None;
                return;
            }
        }
    }
    // Drop any provider's stale per-server schema-cache dir (the ghost surface),
    // keyed by server name = filename minus `.py`. Data-driven: each provider
    // declares its cache subdir via config_dir.cache_subdir (e.g. antigravity's
    // `.gemini/antigravity-cli/mcp/<server>/`) — no per-provider hard-coding.
    let stem = name.strip_suffix(".py").unwrap_or(&name);
    for def in crate::provider_registry::ProviderRegistry::load().all() {
        let cd = &def.provider.config_dir;
        if cd.cache_subdir.is_empty() {
            continue;
        }
        let cache = home.join(&cd.path).join(&cd.cache_subdir).join(stem);
        if cache.is_dir() {
            let _ = std::fs::remove_dir_all(&cache);
        }
    }
    // Unregister from the target workspace's mcp_tools if present.
    if t.enabled.remove(&name) {
        let mut list: Vec<String> = t.enabled.iter().cloned().collect();
        list.sort();
        let _ = crate::config::write_mcp_tools(&t.target, &list);
    }

    t.status = if removed {
        format!("deleted {name} from disk")
    } else {
        format!("{name} had no file on disk — cleaned up")
    };
    t.confirm_delete = None;

    // Rebuild rows from the now-current volume + keep the cursor in range.
    let installed = installed_volume_tools();
    t.rows = build_tool_rows(&avail, &installed, &t.enabled);
    let n = filter_tool_rows(t).len();
    if t.sel >= n {
        t.sel = n.saturating_sub(1);
    }
}

/// Open the Config overlay in `mode`, resolving the active config's tools (which
/// ones still exist) and any stray configs that could shadow sessions.
fn open_config(st: &mut State, mode: ConfigMode) {
    let target = st.cwd_config.clone();
    let target_label = workspace_label(&target);
    let enabled = crate::config::read_mcp_tools(&target);
    let avail: HashSet<&str> = st.avail_tools.iter().map(String::as_str).collect();
    let registry = crate::mcp_registry::McpRegistry::load_host(&crate::paths::data_home());
    let reg: HashSet<String> = registry.names().iter().map(|s| s.to_string()).collect();
    let tools: Vec<(String, bool)> = enabled
        .iter()
        .map(|t| {
            let ok = t.starts_with("http://")
                || t.starts_with("https://")
                || crate::config::is_binary_server(t)
                || avail.contains(t.as_str())
                || reg.contains(t.as_str());
            (t.clone(), ok)
        })
        .collect();
    let cwd = target
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let strays = crate::config::scan_stray_configs(&cwd, Some(&target));
    st.config = Some(ConfigModal {
        mode,
        target,
        target_label,
        tools,
        strays,
        status: String::new(),
        done: false,
    });
}

/// Back a config up to `<path>.bak-<unixsecs>`. `move_it=true` RENAMES (used for
/// stray configs we want emptied so they stop leaking); `move_it=false` COPIES
/// (used for the reset target, so it's never momentarily missing — the original
/// stays in place until it's overwritten).
fn archive_config(path: &Path, move_it: bool) -> std::io::Result<PathBuf> {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let bak = PathBuf::from(format!("{}.bak-{}", path.display(), ts));
    if move_it {
        std::fs::rename(path, &bak)?;
    } else {
        std::fs::copy(path, &bak)?;
    }
    Ok(bak)
}

/// Archive the target config (if present) and write a fresh scaffold. When
/// `reset_strays`, also archive every stray config found (the home-root leak…).
fn do_config_init(st: &mut State, reset_strays: bool) {
    let Some(m) = st.config.as_mut() else { return };
    let target = m.target.clone();
    let dir_name = target
        .parent()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "project".to_string());
    let mut msgs: Vec<String> = Vec::new();

    // COPY the current config aside first (it stays in place), THEN overwrite it
    // — so a failure never leaves the workspace without a config (the bug that
    // wiped research/blender, leaving only .bak files).
    if target.is_file() {
        match archive_config(&target, false) {
            Ok(bak) => msgs.push(format!(
                "archived → {}",
                bak.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
            )),
            Err(e) => {
                m.status = format!("archive failed: {e}");
                return;
            }
        }
    }
    if let Some(parent) = target.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(e) = std::fs::write(&target, crate::config::Config::scaffold_template(&dir_name)) {
        m.status = format!("write failed: {e} (original preserved in .bak)");
        return;
    }
    msgs.push("wrote fresh template".to_string());

    if reset_strays {
        let strays = m.strays.clone();
        let mut n = 0;
        for stray in &strays {
            // MOVE strays aside so their path stops resolving (they're leaks).
            if archive_config(stray, true).is_ok() {
                n += 1;
            }
        }
        if n > 0 {
            msgs.push(format!("archived {n} stray config(s)"));
        }
    }

    m.status = msgs.join(" · ");
    m.done = true;
}

/// Render the Config overlay: a validation report, or an archive/reset confirm.
fn draw_config(f: &mut ratatui::Frame, area: Rect, st: &State) {
    let Some(m) = st.config.as_ref() else { return };
    let w = 72u16.min(area.width.saturating_sub(2));
    let h = 24u16.min(area.height.saturating_sub(2));
    let modal = centered(area, w, h);
    f.render_widget(Clear, modal);
    let title = match m.mode {
        ConfigMode::Validate => "Config · Validate",
        ConfigMode::Init => "Config · Init",
        ConfigMode::Reset => "Config · Archive & Reset",
    };
    f.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan))
            .title(format!("  {title} · {}  ", m.target_label)),
        modal,
    );
    let inner = Rect::new(
        modal.x + 2,
        modal.y + 1,
        modal.width.saturating_sub(4),
        modal.height.saturating_sub(2),
    );
    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(vec![
        Span::styled("config  ", Style::default().fg(Color::DarkGray)),
        Span::styled(m.target.display().to_string(), Style::default().fg(Color::White)),
    ]));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        format!("mcp_tools ({})", m.tools.len()),
        Style::default().fg(Color::Indexed(244)),
    )));
    for (name, ok) in &m.tools {
        let (mark, c) = if *ok { ("✓", Color::Green) } else { ("✗ missing", Color::Red) };
        lines.push(Line::from(vec![
            Span::styled(
                format!("  {name:<28} "),
                Style::default().fg(if *ok { Color::Gray } else { Color::Red }),
            ),
            Span::styled(mark.to_string(), Style::default().fg(c)),
        ]));
    }
    lines.push(Line::from(""));
    if m.strays.is_empty() {
        lines.push(Line::from(Span::styled(
            "no stray configs found",
            Style::default().fg(Color::Green),
        )));
    } else {
        lines.push(Line::from(Span::styled(
            format!("stray configs that can shadow sessions ({}):", m.strays.len()),
            Style::default().fg(Color::Yellow),
        )));
        for s in &m.strays {
            lines.push(Line::from(Span::styled(
                format!("  {}", s.display()),
                Style::default().fg(Color::Yellow),
            )));
        }
    }
    lines.push(Line::from(""));
    let footer = if m.done {
        Span::styled(m.status.clone(), Style::default().fg(Color::Green))
    } else {
        match m.mode {
            ConfigMode::Validate => {
                Span::styled("esc/q close", Style::default().fg(Color::DarkGray))
            }
            ConfigMode::Init => Span::styled(
                "archive this config + write a fresh template?   y / n",
                Style::default().fg(Color::White),
            ),
            ConfigMode::Reset => Span::styled(
                format!(
                    "archive this config + {} stray(s) and reset?   y / n",
                    m.strays.len()
                ),
                Style::default().fg(Color::White),
            ),
        }
    };
    lines.push(Line::from(footer));
    let max = inner.height as usize;
    if lines.len() > max {
        lines.truncate(max);
    }
    f.render_widget(Paragraph::new(lines), inner);
}

/// Render the tools-picker overlay: a scrollable checkbox list of MCP tools.
/// Tools picker geometry — (modal box, scrollable list area). Shared by the
/// renderer and the mouse handler so a click lands on the right row. Mirrors the
/// Layout::vertical([head=1, list=Min, status=1]) split inside the modal.
fn tools_modal_geom(area: Rect) -> (Rect, Rect) {
    let w = 66u16.min(area.width.saturating_sub(2));
    let h = 22u16.min(area.height.saturating_sub(2));
    let modal = centered(area, w, h);
    let list = Rect::new(
        modal.x + 2,
        modal.y + 2, // border(1) + head row(1)
        modal.width.saturating_sub(4),
        modal.height.saturating_sub(4), // minus border×2 + head + status
    );
    (modal, list)
}

fn draw_tools(f: &mut ratatui::Frame, area: Rect, st: &State) {
    let Some(t) = st.tools.as_ref() else { return };
    let w = 66u16.min(area.width.saturating_sub(2));
    let h = 22u16.min(area.height.saturating_sub(2));
    let modal = centered(area, w, h);
    f.render_widget(Clear, modal);
    f.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan))
            .title(format!("  Tools · {}  ", t.target_label)),
        modal,
    );
    let inner = Rect::new(
        modal.x + 2,
        modal.y + 1,
        modal.width.saturating_sub(4),
        modal.height.saturating_sub(2),
    );
    let rows = Layout::vertical([
        Constraint::Length(1), // filter / hint
        Constraint::Min(1),    // list
        Constraint::Length(1), // status
    ])
    .split(inner);

    // Add-server form takes over the body while open.
    if let Some(a) = t.adding.as_ref() {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "add socket MCP server · tab/↑↓ field · enter save · esc cancel",
                Style::default().fg(Color::DarkGray),
            ))),
            rows[0],
        );
        let fields = [
            ("name", &a.name),
            ("url", &a.url),
            ("token env (optional)", &a.token_env),
        ];
        let mut flines: Vec<Line> = Vec::new();
        for (i, (label, val)) in fields.iter().enumerate() {
            let active = a.field == i;
            let style = if active {
                Style::default().fg(Color::White)
            } else {
                Style::default().fg(Color::Gray)
            };
            let caret = if active { ">" } else { " " };
            let cursor = if active { "▏" } else { "" };
            flines.push(Line::from(vec![
                Span::styled(format!("{caret} {label:<20} "), style),
                Span::styled(format!("{val}{cursor}"), style),
            ]));
        }
        flines.push(Line::from(""));
        flines.push(Line::from(Span::styled(
            "token env: host var holding a Bearer token (e.g. HYPERIA_AGENT_TOKEN)",
            Style::default().fg(Color::DarkGray),
        )));
        if !a.error.is_empty() {
            flines.push(Line::from(Span::styled(
                a.error.clone(),
                Style::default().fg(Color::Red),
            )));
        }
        f.render_widget(Paragraph::new(flines), rows[1]);
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!("new server → {}", t.target_label),
                Style::default().fg(Color::Indexed(244)),
            ))),
            rows[2],
        );
        return;
    }

    let head = if t.filtering || !t.filter.is_empty() {
        Span::styled(
            format!("filter: {}▏", t.filter),
            Style::default().fg(Color::White),
        )
    } else {
        Span::styled(
            "space toggle · a add · d delete · / filter · esc close",
            Style::default().fg(Color::DarkGray),
        )
    };
    f.render_widget(Paragraph::new(Line::from(head)), rows[0]);

    let filtered = filter_tool_rows(t);
    let list_h = rows[1].height as usize;
    let offset = if t.sel >= list_h {
        t.sel + 1 - list_h
    } else {
        0
    };
    let mut lines: Vec<Line> = Vec::new();
    for (vis, &ri) in filtered.iter().enumerate().skip(offset).take(list_h) {
        let (name, kind) = &t.rows[ri];
        let checked = t.enabled.contains(name);
        let boxs = match kind {
            ToolKind::Binary => "[●]",
            ToolKind::Stale => "[!]",
            _ if checked => "[x]",
            _ => "[ ]",
        };
        let (tag, tagc) = match kind {
            ToolKind::Builtin => ("", Color::Gray),
            ToolKind::Registry => ("mcp", Color::Magenta),
            ToolKind::Url => ("url", Color::Cyan),
            ToolKind::Extra => ("host", Color::Yellow),
            ToolKind::Binary => ("built-in", Color::Green),
            ToolKind::Stale => ("stale", Color::Red),
        };
        let selected = vis == t.sel;
        let base = if selected {
            Style::default().bg(Color::Indexed(238)).fg(Color::White)
        } else if *kind == ToolKind::Stale {
            Style::default().fg(Color::Red)
        } else if checked || *kind == ToolKind::Binary {
            Style::default().fg(Color::White)
        } else {
            Style::default().fg(Color::Gray)
        };
        let mut spans = vec![
            Span::styled(format!(" {boxs} "), base),
            Span::styled(name.clone(), base),
        ];
        if !tag.is_empty() {
            spans.push(Span::raw("  "));
            spans.push(Span::styled(format!("[{tag}]"), Style::default().fg(tagc)));
        }
        lines.push(Line::from(spans));
    }
    f.render_widget(Paragraph::new(lines), rows[1]);

    let stale = t.rows.iter().filter(|(_, k)| *k == ToolKind::Stale).count();
    let status = if !t.status.is_empty() {
        Span::styled(t.status.clone(), Style::default().fg(Color::Green))
    } else if stale > 0 {
        Span::styled(
            format!("{} enabled · {stale} stale (d to delete)", t.enabled.len()),
            Style::default().fg(Color::Red),
        )
    } else {
        Span::styled(
            format!(
                "{} enabled · {}/{} shown",
                t.enabled.len(),
                filtered.len(),
                t.rows.len()
            ),
            Style::default().fg(Color::Indexed(244)),
        )
    };
    f.render_widget(Paragraph::new(Line::from(status)), rows[2]);
}

// ── input ───────────────────────────────────────────────────────────────────

/// Kill the named container via the runtime CLI, then ask for a refresh.
fn do_kill(st: &mut State, ctx: &Ctx, name: &str) {
    let ok = std::process::Command::new(&ctx.runtime)
        .args(["kill", name])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    st.status = if ok {
        format!("killed {name} — session preserved")
    } else {
        format!("kill {name} failed")
    };
    if let Some(tx) = ctx.refresh_request.as_ref() {
        let _ = tx.send(());
    }
}

/// Open the detail overlay for the current selection. `section` 0=Meta 2=Logs.
fn open_detail(
    st: &mut State,
    ctx: &Ctx,
    running: &[RunningAgent],
    run_idx: &[usize],
    section: u8,
) {
    let logs = if st.tab == 0 {
        run_idx
            .get(st.sel[0])
            .map(|&i| fetch_logs(&ctx.runtime, &running[i].name))
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    st.detail = Some(Detail { section, logs, scroll: 0 });
}

#[allow(clippy::too_many_arguments)]
fn on_key(
    st: &mut State,
    code: KeyCode,
    mods: KeyModifiers,
    ctx: &Ctx,
    running: &[RunningAgent],
    run_idx: &[usize],
    sessions: &[SessionInfo],
    sess_idx: &[usize],
    last: usize,
) -> Option<Flow> {
    // Kill-confirm modal swallows everything until answered (v3 §3.8).
    if let Some(name) = st.confirm_kill.clone() {
        match code {
            KeyCode::Enter | KeyCode::Char('k') => {
                st.confirm_kill = None;
                do_kill(st, ctx, &name);
            }
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('n') => st.confirm_kill = None,
            _ => {}
        }
        return Some(Flow::Continue);
    }

    // New-session modal swallows all keys until closed/launched.
    if st.modal.is_some() {
        let np = st.providers.len().max(1);
        let model_opts = st.model_options();
        let mut confirm = false;
        let mut close = false;
        {
            let m = st.modal.as_mut().unwrap();
            if m.dd_open {
                match code {
                    KeyCode::Esc => m.dd_open = false,
                    KeyCode::Up => m.dd_sel = (m.dd_sel + np - 1) % np,
                    KeyCode::Down => m.dd_sel = (m.dd_sel + 1) % np,
                    KeyCode::Enter => {
                        m.provider_idx = m.dd_sel;
                        m.dd_open = false;
                        m.mdd_sel = 0;
                    }
                    _ => {}
                }
                return Some(Flow::Continue);
            }
            if m.mdd_open {
                let rows = model_opts.len() + 1; // row 0 = "default"
                match code {
                    KeyCode::Esc => m.mdd_open = false,
                    KeyCode::Up => m.mdd_sel = (m.mdd_sel + rows - 1) % rows,
                    KeyCode::Down => m.mdd_sel = (m.mdd_sel + 1) % rows,
                    KeyCode::Enter => {
                        m.model = if m.mdd_sel == 0 {
                            String::new()
                        } else {
                            model_opts[m.mdd_sel - 1].0.clone()
                        };
                        m.mdd_open = false;
                    }
                    // Typing overrides: close the pulldown, go free-text.
                    KeyCode::Char(c) => {
                        m.mdd_open = false;
                        m.model.push(c);
                    }
                    KeyCode::Backspace => {
                        m.mdd_open = false;
                        m.model.pop();
                    }
                    _ => {}
                }
                return Some(Flow::Continue);
            }
            match code {
                KeyCode::Esc => close = true,
                KeyCode::Tab | KeyCode::Down => m.focus = next_field(m.focus),
                KeyCode::BackTab | KeyCode::Up => m.focus = prev_field(m.focus),
                KeyCode::Left | KeyCode::Right => match m.focus {
                    MField::Provider => {
                        m.provider_idx = if matches!(code, KeyCode::Left) {
                            (m.provider_idx + np - 1) % np
                        } else {
                            (m.provider_idx + 1) % np
                        };
                        m.mdd_sel = 0;
                    }
                    MField::Danger => m.danger = !m.danger,
                    MField::Launch => m.focus = MField::Cancel,
                    MField::Cancel => m.focus = MField::Launch,
                    MField::Model => {}
                },
                KeyCode::Char(' ') if m.focus == MField::Danger => m.danger = !m.danger,
                KeyCode::Char(c) if m.focus == MField::Model => m.model.push(c),
                KeyCode::Backspace if m.focus == MField::Model => {
                    m.model.pop();
                }
                KeyCode::Enter => match m.focus {
                    MField::Provider => {
                        m.dd_open = true;
                        m.dd_sel = m.provider_idx;
                    }
                    // Model field: a populated catalog opens the pulldown;
                    // no catalog → Enter launches like every other field.
                    MField::Model if !model_opts.is_empty() => {
                        m.mdd_open = true;
                        m.mdd_sel = model_opts
                            .iter()
                            .position(|(id, _)| *id == m.model)
                            .map(|p| p + 1)
                            .unwrap_or(0);
                    }
                    MField::Cancel => close = true,
                    _ => confirm = true,
                },
                _ => {}
            }
        }
        if confirm {
            return Some(confirm_modal(st));
        }
        if close {
            st.modal = None;
        }
        return Some(Flow::Continue);
    }

    // Config overlay swallows keys until closed. Confirm modes (Init/Reset) act
    // on y/Enter; everything closes on esc/q/n (or any key once the action ran).
    if st.config.is_some() {
        let awaiting = {
            let m = st.config.as_ref().unwrap();
            !m.done && m.mode != ConfigMode::Validate
        };
        match code {
            KeyCode::Char('y') | KeyCode::Enter if awaiting => {
                let reset = st.config.as_ref().unwrap().mode == ConfigMode::Reset;
                do_config_init(st, reset);
            }
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('n') | KeyCode::Enter => {
                st.config = None;
            }
            _ => {}
        }
        return Some(Flow::Continue);
    }

    // Tools picker swallows keys until closed. Each toggle persists to the
    // target workspace's .nemesis8.toml immediately, so the change is in effect
    // the next time that workspace launches — New or Resume alike (Attach can't
    // change a live container, so the picker is never offered for it).
    if st.tools.is_some() {
        // Add-server form swallows keys until submitted/cancelled.
        if st.tools.as_ref().unwrap().adding.is_some() {
            match code {
                KeyCode::Esc => st.tools.as_mut().unwrap().adding = None,
                KeyCode::Tab | KeyCode::Down => {
                    let a = st.tools.as_mut().unwrap().adding.as_mut().unwrap();
                    a.field = (a.field + 1) % 3;
                }
                KeyCode::BackTab | KeyCode::Up => {
                    let a = st.tools.as_mut().unwrap().adding.as_mut().unwrap();
                    a.field = (a.field + 2) % 3;
                }
                KeyCode::Backspace => {
                    st.tools.as_mut().unwrap().adding.as_mut().unwrap().current_mut().pop();
                }
                KeyCode::Char(c) => {
                    st.tools.as_mut().unwrap().adding.as_mut().unwrap().current_mut().push(c);
                }
                KeyCode::Enter => submit_add_server(st),
                _ => {}
            }
            return Some(Flow::Continue);
        }
        if st.tools.as_ref().unwrap().filtering {
            let t = st.tools.as_mut().unwrap();
            match code {
                KeyCode::Esc => { t.filter.clear(); t.filtering = false; t.sel = 0; }
                KeyCode::Enter => t.filtering = false,
                KeyCode::Backspace => { t.filter.pop(); t.sel = 0; }
                KeyCode::Char(c) => { t.filter.push(c); t.sel = 0; }
                _ => {}
            }
            return Some(Flow::Continue);
        }
        let n = filter_tool_rows(st.tools.as_ref().unwrap()).len();
        let last_row = n.saturating_sub(1);
        match code {
            // Esc cancels an armed delete first, then closes on a second press.
            KeyCode::Esc => {
                let t = st.tools.as_mut().unwrap();
                if t.confirm_delete.take().is_some() {
                    t.status = "delete cancelled".to_string();
                } else {
                    st.tools = None;
                }
            }
            KeyCode::Char('q') => st.tools = None,
            KeyCode::Char('/') => {
                let t = st.tools.as_mut().unwrap();
                t.confirm_delete = None;
                t.filtering = true;
            }
            KeyCode::Char(' ') | KeyCode::Enter => {
                st.tools.as_mut().unwrap().confirm_delete = None;
                toggle_tool(st);
            }
            // a: add a socket (HTTP/SSE) MCP server to the registry + enable it.
            KeyCode::Char('a') => {
                let t = st.tools.as_mut().unwrap();
                t.confirm_delete = None;
                t.adding = Some(AddServerInput::new());
            }
            // d: delete the highlighted .py from the volume drawer (arms, then
            // confirms on a second d or y). The whole point of this picker.
            KeyCode::Char('d') => request_delete(st),
            KeyCode::Char('y') => {
                if st.tools.as_ref().unwrap().confirm_delete.is_some() {
                    delete_tool(st);
                }
            }
            KeyCode::Char('n') => {
                let t = st.tools.as_mut().unwrap();
                if t.confirm_delete.take().is_some() {
                    t.status = "delete cancelled".to_string();
                }
            }
            // Navigation clears any armed delete so y/d can't hit a moved row.
            KeyCode::Up => { let t = st.tools.as_mut().unwrap(); t.confirm_delete = None; t.sel = t.sel.saturating_sub(1); }
            KeyCode::Down => { let t = st.tools.as_mut().unwrap(); t.confirm_delete = None; if t.sel < last_row { t.sel += 1; } }
            KeyCode::PageUp => { let t = st.tools.as_mut().unwrap(); t.confirm_delete = None; t.sel = t.sel.saturating_sub(10); }
            KeyCode::PageDown => { let t = st.tools.as_mut().unwrap(); t.confirm_delete = None; t.sel = (t.sel + 10).min(last_row); }
            KeyCode::Home => { let t = st.tools.as_mut().unwrap(); t.confirm_delete = None; t.sel = 0; }
            KeyCode::End => { let t = st.tools.as_mut().unwrap(); t.confirm_delete = None; t.sel = last_row; }
            _ => {}
        }
        return Some(Flow::Continue);
    }

    // Help overlay swallows keys until closed.
    if st.help.is_some() {
        if matches!(code, KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter) {
            st.help = None;
        }
        return Some(Flow::Continue);
    }
    // Detail overlay: sections, log scrolling, and row actions (v3 §3.4).
    if st.detail.is_some() {
        match code {
            KeyCode::Esc => st.detail = None,
            KeyCode::Char('m') => {
                if let Some(d) = st.detail.as_mut() {
                    d.section = 0;
                }
            }
            KeyCode::Char('t') => {
                if let Some(d) = st.detail.as_mut() {
                    d.section = 1;
                }
            }
            KeyCode::Char('l') => {
                if let Some(d) = st.detail.as_mut() {
                    d.section = 2;
                }
            }
            KeyCode::Up => {
                if let Some(d) = st.detail.as_mut() {
                    d.scroll = (d.scroll + 1).min(d.logs.len());
                }
            }
            KeyCode::Down => {
                if let Some(d) = st.detail.as_mut() {
                    d.scroll = d.scroll.saturating_sub(1);
                }
            }
            KeyCode::PageUp => {
                if let Some(d) = st.detail.as_mut() {
                    d.scroll = (d.scroll + 10).min(d.logs.len());
                }
            }
            KeyCode::PageDown => {
                if let Some(d) = st.detail.as_mut() {
                    d.scroll = d.scroll.saturating_sub(10);
                }
            }
            KeyCode::End => {
                if let Some(d) = st.detail.as_mut() {
                    d.scroll = 0; // re-follow the tail
                }
            }
            KeyCode::Char('k') if st.tab == 0 => {
                if let Some(&i) = run_idx.get(st.sel[0]) {
                    st.confirm_kill = Some(running[i].name.clone());
                }
            }
            KeyCode::Char('a') | KeyCode::Enter => {
                return Some(activate(st, running, run_idx, sessions, sess_idx, false))
            }
            KeyCode::Char('.') => {
                return Some(activate(st, running, run_idx, sessions, sess_idx, true))
            }
            _ => {}
        }
        return Some(Flow::Continue);
    }

    // Menu navigation takes priority when a dropdown is open.
    if let Some(mi) = st.menu_open {
        let n = MENUS[mi].1.len();
        match code {
            KeyCode::Esc => st.menu_open = None,
            KeyCode::Left => { st.menu_open = Some((mi + MENUS.len() - 1) % MENUS.len()); st.menu_sel = 0; }
            KeyCode::Right => { st.menu_open = Some((mi + 1) % MENUS.len()); st.menu_sel = 0; }
            KeyCode::Up => st.menu_sel = (st.menu_sel + n - 1) % n,
            KeyCode::Down => st.menu_sel = (st.menu_sel + 1) % n,
            KeyCode::Enter => return Some(menu_select(st, mi, st.menu_sel)),
            _ => {}
        }
        return Some(Flow::Continue);
    }

    // Filter editing.
    if st.filtering {
        match code {
            KeyCode::Esc => { st.filtering = false; st.query.clear(); st.sel[st.tab] = 0; }
            KeyCode::Backspace => { st.query.pop(); st.sel[st.tab] = 0; }
            KeyCode::Char(c) => { st.query.push(c); st.sel[st.tab] = 0; }
            KeyCode::Enter => return Some(activate(st, running, run_idx, sessions, sess_idx, false)),
            KeyCode::Up | KeyCode::Down => {} // fallthrough below
            _ => return Some(Flow::Continue),
        }
        if !matches!(code, KeyCode::Up | KeyCode::Down) {
            return Some(Flow::Continue);
        }
    }

    // Alt+letter opens a menu.
    if mods.contains(KeyModifiers::ALT) {
        if let KeyCode::Char(c) = code {
            if let Some(i) = MENUS.iter().position(|(t, _)| t.to_lowercase().starts_with(c.to_ascii_lowercase())) {
                st.menu_open = Some(i);
                st.menu_sel = 0;
                return Some(Flow::Continue);
            }
        }
    }

    match code {
        KeyCode::Char('q') | KeyCode::Esc => return Some(Flow::Return(None)),
        KeyCode::Char('n') => st.open_modal(),
        KeyCode::Char('t') => {
            let tgt = tools_target(st, sessions, sess_idx);
            open_tools_for(st, tgt);
        }
        KeyCode::Char('/') => st.filtering = true,
        KeyCode::Tab | KeyCode::BackTab => st.tab = 1 - st.tab,
        KeyCode::Char('1') => st.tab = 0,
        KeyCode::Char('2') => st.tab = 1,
        // NOTE: vim j/k movement gave way to the lifecycle verbs (k = kill was
        // an explicit owner ask). Arrows / PgUp / Home remain for movement.
        KeyCode::Up => st.sel[st.tab] = st.sel[st.tab].saturating_sub(1),
        KeyCode::Down => { if st.sel[st.tab] < last { st.sel[st.tab] += 1; } }
        KeyCode::PageUp => st.sel[st.tab] = st.sel[st.tab].saturating_sub(10),
        KeyCode::PageDown => st.sel[st.tab] = (st.sel[st.tab] + 10).min(last),
        KeyCode::Home | KeyCode::Char('g') => st.sel[st.tab] = 0,
        KeyCode::End | KeyCode::Char('G') => st.sel[st.tab] = last,
        KeyCode::Char('k') if st.tab == 0 => {
            if let Some(&i) = run_idx.get(st.sel[0]) {
                st.confirm_kill = Some(running[i].name.clone());
            }
        }
        KeyCode::Char('l') if st.tab == 0 => open_detail(st, ctx, running, run_idx, 2),
        KeyCode::Char('r') => {
            if let Some(tx) = ctx.refresh_request.as_ref() {
                let _ = tx.send(());
                st.status = "refreshing…".to_string();
            }
        }
        KeyCode::Char('a') => return Some(activate(st, running, run_idx, sessions, sess_idx, false)),
        KeyCode::Char('.') => return Some(activate(st, running, run_idx, sessions, sess_idx, true)),
        KeyCode::Enter => open_detail(st, ctx, running, run_idx, if st.tab == 0 { 2 } else { 0 }),
        _ => {}
    }
    Some(Flow::Continue)
}

/// Turn the highlighted row into an action. `current_dir` only affects resume.
fn activate(
    st: &State,
    running: &[RunningAgent],
    run_idx: &[usize],
    sessions: &[SessionInfo],
    sess_idx: &[usize],
    current_dir: bool,
) -> Flow {
    if st.tab == 0 {
        if let Some(&i) = run_idx.get(st.sel[0]) {
            return Flow::Return(Some(Outcome::Attach(running[i].name.clone())));
        }
    } else if let Some(&j) = sess_idx.get(st.sel[1]) {
        return Flow::Return(Some(Outcome::Resume {
            session: sessions[j].clone(),
            current_dir,
        }));
    }
    Flow::Continue
}

/// Handle a menu item selection.
fn menu_select(st: &mut State, menu: usize, item: usize) -> Flow {
    st.menu_open = None;
    match menu {
        0 => match item {
            // Session
            0 => st.open_modal(), // New session → modal
            1 => st.filtering = true, // Find
            _ => {}
        },
        1 => match item {
            // Config
            0 => open_tools_for(st, st.cwd_config.clone()), // Edit tools (cwd)
            1 => return Flow::Return(Some(Outcome::Build)), // Build image (exits TUI)
            2 => open_config(st, ConfigMode::Validate),
            3 => open_config(st, ConfigMode::Init),
            4 => open_config(st, ConfigMode::Reset),
            _ => {}
        },
        2 => st.help = Some(if item == 0 { 1 } else { 2 }), // Help: Keys / About
        _ => {}
    }
    Flow::Continue
}

#[allow(clippy::too_many_arguments)]
fn on_mouse(
    st: &mut State,
    m: event::MouseEvent,
    area: Rect,
    bar_r: Rect,
    tabs_r: Rect,
    table_r: Rect,
    ctx: &Ctx,
    running: &[RunningAgent],
    run_idx: &[usize],
    last: usize,
) -> Option<Flow> {
    let (col, row) = (m.column, m.row);
    // Tools picker: click a row to toggle it, wheel to scroll, click-outside to
    // close. The add-server form is keyboard-only, so swallow clicks there.
    if st.tools.is_some() {
        if st.tools.as_ref().unwrap().adding.is_some() {
            return Some(Flow::Continue);
        }
        let (modal, list) = tools_modal_geom(area);
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if !hit(modal, col, row) {
                    st.tools = None; // click outside closes
                    return Some(Flow::Continue);
                }
                // Map a click in the list area to the row that was rendered there
                // (same scroll offset draw_tools uses), then toggle it.
                let target = {
                    let t = st.tools.as_ref().unwrap();
                    if row >= list.y
                        && row < list.y.saturating_add(list.height)
                        && hit_col(list, col)
                    {
                        let list_h = list.height as usize;
                        let offset = if t.sel >= list_h { t.sel + 1 - list_h } else { 0 };
                        let vis = (row - list.y) as usize;
                        let filtered = filter_tool_rows(t);
                        filtered.get(offset + vis).map(|_| offset + vis)
                    } else {
                        None
                    }
                };
                if let Some(sel) = target {
                    let t = st.tools.as_mut().unwrap();
                    t.sel = sel;
                    t.confirm_delete = None;
                    toggle_tool(st);
                }
            }
            MouseEventKind::ScrollDown => {
                let t = st.tools.as_mut().unwrap();
                let n = filter_tool_rows(t).len();
                if t.sel + 1 < n {
                    t.sel += 1;
                }
            }
            MouseEventKind::ScrollUp => {
                let t = st.tools.as_mut().unwrap();
                t.sel = t.sel.saturating_sub(1);
            }
            _ => {}
        }
        return Some(Flow::Continue);
    }
    // Kill-confirm modal grabs the mouse first.
    if let Some(name) = st.confirm_kill.clone() {
        if let MouseEventKind::Down(MouseButton::Left) = m.kind {
            let (fr, kb, cb) = confirm_rects(area);
            if hit(kb, col, row) {
                st.confirm_kill = None;
                do_kill(st, ctx, &name);
            } else if hit(cb, col, row) || !hit(fr, col, row) {
                st.confirm_kill = None;
            }
        }
        return Some(Flow::Continue);
    }
    // Modal grabs the mouse first.
    if st.modal.is_some() {
        if let MouseEventKind::Down(MouseButton::Left) = m.kind {
            let (modal, pr, mr, dr, lb, cb) = modal_rects(area);
            let dd_open = st.modal.as_ref().map(|x| x.dd_open).unwrap_or(false);
            let mdd_open = st.modal.as_ref().map(|x| x.mdd_open).unwrap_or(false);
            let model_opts = st.model_options();
            if dd_open {
                // Dropdown is a bordered block at pr.y+1, so the first item
                // renders one row down (inside the top border) at pr.y+2.
                let top = pr.y + 2;
                let np = st.providers.len();
                if row >= top && (row as usize) < top as usize + np && hit_col(pr, col) {
                    let i = (row - top) as usize;
                    if let Some(mm) = st.modal.as_mut() {
                        mm.provider_idx = i;
                        mm.dd_open = false;
                        mm.mdd_sel = 0;
                    }
                } else if let Some(mm) = st.modal.as_mut() {
                    mm.dd_open = false;
                }
            } else if mdd_open {
                // Same bordered-block geometry as the provider pulldown:
                // first row (the "default" entry) is at mr.y+2.
                let top = mr.y + 2;
                let rows = model_opts.len() + 1;
                if row >= top && (row as usize) < top as usize + rows && hit_col(mr, col) {
                    let i = (row - top) as usize;
                    if let Some(mm) = st.modal.as_mut() {
                        mm.model = if i == 0 {
                            String::new()
                        } else {
                            model_opts[i - 1].0.clone()
                        };
                        mm.mdd_open = false;
                    }
                } else if let Some(mm) = st.modal.as_mut() {
                    mm.mdd_open = false;
                }
            } else if hit(pr, col, row) {
                if let Some(mm) = st.modal.as_mut() {
                    mm.focus = MField::Provider;
                    mm.dd_open = true;
                    mm.dd_sel = mm.provider_idx;
                }
            } else if hit(mr, col, row) {
                let has = !model_opts.is_empty();
                if let Some(mm) = st.modal.as_mut() {
                    mm.focus = MField::Model;
                    if has {
                        mm.mdd_open = true;
                        mm.mdd_sel = model_opts
                            .iter()
                            .position(|(id, _)| *id == mm.model)
                            .map(|p| p + 1)
                            .unwrap_or(0);
                    }
                }
            } else if hit(dr, col, row) {
                if let Some(mm) = st.modal.as_mut() {
                    mm.danger = !mm.danger;
                    mm.focus = MField::Danger;
                }
            } else if hit(lb, col, row) {
                return Some(confirm_modal(st));
            } else if hit(cb, col, row) || !hit(modal, col, row) {
                st.modal = None;
            }
        }
        return Some(Flow::Continue);
    }
    // Overlays grab the mouse: a click dismisses them.
    if st.help.is_some() {
        if matches!(m.kind, MouseEventKind::Down(_)) {
            st.help = None;
        }
        return Some(Flow::Continue);
    }
    if st.detail.is_some() {
        match m.kind {
            // Wheel scrolls the log tail (running-agent detail).
            MouseEventKind::ScrollUp => {
                if let Some(d) = st.detail.as_mut() {
                    d.scroll = (d.scroll + 3).min(d.logs.len());
                }
            }
            MouseEventKind::ScrollDown => {
                if let Some(d) = st.detail.as_mut() {
                    d.scroll = d.scroll.saturating_sub(3);
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                // Click on the section tabs in the title row switches section;
                // any other click closes the overlay.
                let dr = detail_rect(area);
                if st.tab == 0 && row == dr.y && hit(dr, col, row) {
                    // Title layout: "  name · prov  Meta  Tools  Logs " — pick
                    // the section by thirds of the right half of the title.
                    let third = dr.width / 6;
                    let zone = col.saturating_sub(dr.x + dr.width / 2) / third.max(1);
                    if col >= dr.x + dr.width / 2 {
                        if let Some(d) = st.detail.as_mut() {
                            d.section = (zone as u8).min(2);
                        }
                        return Some(Flow::Continue);
                    }
                }
                st.detail = None;
            }
            _ => {}
        }
        return Some(Flow::Continue);
    }
    match m.kind {
        MouseEventKind::ScrollDown => {
            st.sel[st.tab] = (st.sel[st.tab] + 3).min(last);
        }
        MouseEventKind::ScrollUp => {
            st.sel[st.tab] = st.sel[st.tab].saturating_sub(3);
        }
        MouseEventKind::Down(MouseButton::Left) => {
            // Menu bar?
            if row == bar_r.y {
                if let Some(i) = menu_at(st, col) {
                    st.menu_open = if st.menu_open == Some(i) { None } else { Some(i) };
                    st.menu_sel = 0;
                    return Some(Flow::Continue);
                }
                st.menu_open = None;
                return Some(Flow::Continue);
            }
            // Open dropdown item?
            if let Some(mi) = st.menu_open {
                let items = MENUS[mi].1;
                let x = *st.menu_x.get(mi).unwrap_or(&bar_r.x);
                let top = bar_r.y + 2; // first item row (inside the border)
                if row >= top && (row as usize) < top as usize + items.len() && col >= x {
                    return Some(menu_select(st, mi, (row - top) as usize));
                }
                st.menu_open = None;
                return Some(Flow::Continue);
            }
            // Tab strip? Running (left) vs Sessions.
            if row == tabs_r.y {
                st.tab = if (col as usize) < (tabs_r.x as usize + 12) { 0 } else { 1 };
                return Some(Flow::Continue);
            }
            // Table row? rows start at table_r.y + 2 (border + header).
            let first = table_r.y + 2;
            if row >= first && row < table_r.y + table_r.height.saturating_sub(1) {
                let visible_idx = (row - first) as usize;
                let offset = st.tstate[st.tab].offset();
                let sel = offset + visible_idx;
                if sel <= last {
                    st.sel[st.tab] = sel;
                    // Click a row → open its detail (logs for running agents).
                    open_detail(st, ctx, running, run_idx, if st.tab == 0 { 2 } else { 0 });
                }
            }
        }
        _ => {}
    }
    Some(Flow::Continue)
}

fn menu_at(st: &State, col: u16) -> Option<usize> {
    for (i, (title, _)) in MENUS.iter().enumerate() {
        let start = *st.menu_x.get(i)?;
        let end = start + title.len() as u16 + 2;
        if col >= start && col < end {
            return Some(i);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_model_catalog_parses_endpoint_shape() {
        // Mirrors the live nemesis8.nuts.services/models envelope.
        let json = r#"{
            "generated_at": "2026-06-12T20:47:13Z",
            "ttl_seconds": 3600,
            "providers": {
                "claude": {
                    "ok": true,
                    "default": "claude-sonnet-4-6",
                    "models": [
                        {"id": "claude-opus-4-8", "label": "Claude Opus 4.8"},
                        {"id": "claude-sonnet-4-6", "label": "Claude Sonnet 4.6"}
                    ]
                },
                "grok": {"ok": false, "error": "XAI_API_KEY not configured", "models": []}
            }
        }"#;
        let cat: ModelCatalog = serde_json::from_str(json).unwrap();
        assert_eq!(cat.ttl_seconds, 3600);
        let claude = &cat.providers["claude"];
        assert!(claude.ok);
        assert_eq!(claude.default.as_deref(), Some("claude-sonnet-4-6"));
        assert_eq!(claude.models.len(), 2);
        assert_eq!(claude.models[0].id, "claude-opus-4-8");
        assert!(!cat.providers["grok"].ok);
    }

    #[test]
    fn test_bar_height_wraps_on_narrow_terminals() {
        // Wide terminal: one row. Narrow: wraps, but never more than 3.
        assert_eq!(bar_height(Bar::Bot, 200), 1);
        assert!(bar_height(Bar::Top, 40) >= 2);
        assert!(bar_height(Bar::Top, 10) <= 3);
    }
}
