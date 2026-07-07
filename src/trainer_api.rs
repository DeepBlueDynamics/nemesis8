//! Sailfish trainer API — tool-run training data over localhost HTTP.
//!
//! Part 1 of the Sailfish ↔ nemesis8 handoff (sailfish/NEMESIS8_INTEGRATION.md):
//! Sailfish must never parse agent logs itself — nemesis8 owns the logs and
//! serves the training atoms. Endpoints (port 9802, 127.0.0.1 ONLY, optional
//! bearer via SAILFISH_N8_TOKEN):
//!
//!   GET  /v1/providers      which tool-bags have tool-run data (+counts)
//!   GET  /v1/tools/search   find tools across all logs (q/provider/tool/limit)
//!   GET  /v1/tool-runs      stream the runs as JSONL — the training data
//!   GET  /v1/stats          tool histogram + arg-key frequencies
//!   GET  /v1/appliance      Sailfish appliance container status
//!   POST /v1/appliance/ensure   install/start the appliance
//!
//! A "tool-run" is one invocation: lead-up context (last 6 text messages), the
//! tool, its arguments, and a scrubbed result preview. Providers are derived
//! from the call itself: `mcp__hyperia__terminal_run` → provider `hyperia`,
//! tool `terminal_run`; plain built-ins (Bash/Edit/…) → provider `claude-code`.
//!
//! Sources: the host's `~/.claude/projects/**/*.jsonl` plus the agents' data
//! home (`~/.nemesis8/home/.claude/projects`) — read in place, never uploaded.
//! The index is per-file incremental (mtime+size), so only changed sessions
//! re-parse on refresh.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

/// The wired default port (Sailfish reaches us at host.docker.internal:9802 — Sailfish must set SAILFISH_N8_URL accordingly; 18042 was inside the chisel exposure range 18000-18999).
pub const TRAINER_PORT: u16 = 9802;

/// Context window: how many lead-up text messages each run carries.
const CTX_WINDOW: usize = 6;
/// Cap a single context message (chars) so records stay lean.
const CTX_MSG_CAP: usize = 4000;
/// Result preview length (chars), per the spec.
const PREVIEW_CAP: usize = 400;

// ---------------------------------------------------------------------------
// Record shape (§1.3 of the spec — stable across providers)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct CtxMsg {
    pub role: String,
    pub text: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolRun {
    pub provider: String,
    pub tool: String,
    pub arguments: serde_json::Value,
    pub context: Vec<CtxMsg>,
    pub result_preview: String,
    pub ts: String,
}

/// `mcp__hyperia__terminal_run` → ("hyperia", "terminal_run");
/// anything else → ("claude-code", name). The provider IS the tool-bag.
pub fn split_tool(name: &str) -> (String, String) {
    if let Some(rest) = name.strip_prefix("mcp__") {
        if let Some((server, tool)) = rest.split_once("__") {
            return (server.to_string(), tool.to_string());
        }
        return (rest.to_string(), rest.to_string());
    }
    ("claude-code".to_string(), name.to_string())
}

/// Strip ANSI CSI/OSC escapes, CR, NUL and other C0 controls (keeps \n, \t).
/// Tool results from Bash/PowerShell/terminal_screen are the mess; clean at
/// the source so trainers never see control bytes.
pub fn scrub(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => match chars.peek() {
                // CSI: ESC [ params… final-byte (0x40–0x7e)
                Some('[') => {
                    chars.next();
                    for n in chars.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&n) {
                            break;
                        }
                    }
                }
                // OSC: ESC ] … terminated by BEL or ST (ESC \)
                Some(']') => {
                    chars.next();
                    while let Some(n) = chars.next() {
                        if n == '\u{07}' {
                            break;
                        }
                        if n == '\u{1b}' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                // Two-char escape (ESC X)
                _ => {
                    chars.next();
                }
            },
            '\r' | '\0' => {}
            c if c.is_control() && c != '\n' && c != '\t' => {}
            c => out.push(c),
        }
    }
    out
}

fn cap_chars(s: &str, cap: usize) -> String {
    if s.chars().count() <= cap {
        s.to_string()
    } else {
        s.chars().take(cap).collect()
    }
}

// ---------------------------------------------------------------------------
// Extraction — stream one session .jsonl into ToolRuns
// ---------------------------------------------------------------------------

fn push_ctx(window: &mut VecDeque<CtxMsg>, role: &str, text: &str, clean: bool) {
    let t = if clean { scrub(text) } else { text.to_string() };
    let t = cap_chars(t.trim(), CTX_MSG_CAP);
    if t.is_empty() {
        return;
    }
    window.push_back(CtxMsg { role: role.to_string(), text: t });
    while window.len() > CTX_WINDOW {
        window.pop_front();
    }
}

/// Flatten a tool_result `content` (string or array of text items) to a
/// scrubbed preview.
fn result_preview(content: Option<&serde_json::Value>, clean: bool) -> String {
    let raw = match content {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Array(items)) => items
            .iter()
            .filter_map(|i| i.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    };
    let cleaned = if clean { scrub(&raw) } else { raw };
    cap_chars(cleaned.trim(), PREVIEW_CAP)
}

/// Walk one session file, calling `on_run` for every completed tool-run.
/// Runs whose result never arrives (file tail, crashes) are emitted with an
/// empty preview at EOF — the context+arguments are still training data.
pub fn extract_file(
    path: &Path,
    clean: bool,
    mut on_run: impl FnMut(ToolRun),
) -> std::io::Result<()> {
    use std::io::BufRead;
    let f = std::fs::File::open(path)?;
    let reader = std::io::BufReader::new(f);

    let mut window: VecDeque<CtxMsg> = VecDeque::new();
    // tool_use id → pending run awaiting its tool_result. Ordered leftovers
    // flush at EOF, so keep insertion order.
    let mut pending: Vec<(String, ToolRun)> = Vec::new();

    for line in reader.lines() {
        let Ok(line) = line else { continue };
        let Ok(val) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        let ts = val
            .get("timestamp")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string();
        let rtype = val.get("type").and_then(|t| t.as_str()).unwrap_or("");
        let Some(message) = val.get("message") else { continue };
        let content = message.get("content");

        match rtype {
            "user" => {
                if let Some(text) = content.and_then(|c| c.as_str()) {
                    push_ctx(&mut window, "user", text, clean);
                } else if let Some(items) = content.and_then(|c| c.as_array()) {
                    for item in items {
                        match item.get("type").and_then(|t| t.as_str()) {
                            Some("text") => {
                                if let Some(t) = item.get("text").and_then(|t| t.as_str()) {
                                    push_ctx(&mut window, "user", t, clean);
                                }
                            }
                            Some("tool_result") => {
                                let Some(id) =
                                    item.get("tool_use_id").and_then(|i| i.as_str())
                                else {
                                    continue;
                                };
                                if let Some(pos) = pending.iter().position(|(pid, _)| pid == id)
                                {
                                    let (_, mut run) = pending.remove(pos);
                                    run.result_preview =
                                        result_preview(item.get("content"), clean);
                                    on_run(run);
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
            "assistant" => {
                let Some(items) = content.and_then(|c| c.as_array()) else {
                    continue;
                };
                // Text blocks in THIS message that precede a tool_use are part
                // of its lead-up (the model's own reasoning before the call).
                let mut cur_text = String::new();
                for item in items {
                    match item.get("type").and_then(|t| t.as_str()) {
                        Some("text") => {
                            if let Some(t) = item.get("text").and_then(|t| t.as_str()) {
                                if !cur_text.is_empty() {
                                    cur_text.push('\n');
                                }
                                cur_text.push_str(t);
                            }
                        }
                        Some("tool_use") => {
                            let Some(name) = item.get("name").and_then(|n| n.as_str()) else {
                                continue;
                            };
                            let (provider, tool) = split_tool(name);
                            let mut context: Vec<CtxMsg> = window.iter().cloned().collect();
                            if !cur_text.trim().is_empty() {
                                let t = if clean { scrub(&cur_text) } else { cur_text.clone() };
                                context.push(CtxMsg {
                                    role: "assistant".to_string(),
                                    text: cap_chars(t.trim(), CTX_MSG_CAP),
                                });
                            }
                            let run = ToolRun {
                                provider,
                                tool,
                                arguments: item
                                    .get("input")
                                    .cloned()
                                    .unwrap_or_else(|| serde_json::json!({})),
                                context,
                                result_preview: String::new(),
                                ts: ts.clone(),
                            };
                            match item.get("id").and_then(|i| i.as_str()) {
                                Some(id) if !id.is_empty() => {
                                    pending.push((id.to_string(), run))
                                }
                                _ => on_run(run),
                            }
                        }
                        _ => {}
                    }
                }
                if !cur_text.trim().is_empty() {
                    push_ctx(&mut window, "assistant", &cur_text, clean);
                }
            }
            _ => {}
        }
    }

    for (_, run) in pending {
        on_run(run);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Index — per-file incremental aggregates (mtime+size keyed)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
struct PairAgg {
    runs: u64,
    last: String,
    arg_keys: HashMap<String, u64>,
}

#[derive(Debug, Clone)]
struct FileAgg {
    mtime: std::time::SystemTime,
    size: u64,
    /// (provider, tool) → aggregate
    pairs: HashMap<(String, String), PairAgg>,
}

#[derive(Default)]
pub struct Index {
    files: HashMap<PathBuf, FileAgg>,
}

/// The transcript roots we serve: host Claude Code sessions + the agents'
/// data-home sessions. Read-only, in place.
pub fn source_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(home) = dirs::home_dir() {
        roots.push(home.join(".claude").join("projects"));
    }
    roots.push(crate::paths::data_home().join(".claude").join("projects"));
    roots
}

fn walk_jsonl(root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(root) else { return };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            walk_jsonl(&p, out);
        } else if p.extension().is_some_and(|x| x == "jsonl") {
            out.push(p);
        }
    }
}

impl Index {
    /// Incremental refresh: stat every .jsonl under the roots, re-parse only
    /// new/changed files, drop vanished ones. Returns (files, reparsed).
    pub fn refresh(&mut self, roots: &[PathBuf]) -> (usize, usize) {
        let mut found = Vec::new();
        for root in roots {
            walk_jsonl(root, &mut found);
        }
        let live: HashSet<&PathBuf> = found.iter().collect();
        self.files.retain(|p, _| live.contains(p));

        let mut reparsed = 0usize;
        for path in &found {
            let Ok(meta) = std::fs::metadata(path) else { continue };
            let mtime = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
            let size = meta.len();
            if let Some(agg) = self.files.get(path) {
                if agg.mtime == mtime && agg.size == size {
                    continue;
                }
            }
            let mut pairs: HashMap<(String, String), PairAgg> = HashMap::new();
            let _ = extract_file(path, true, |run| {
                let agg = pairs
                    .entry((run.provider.clone(), run.tool.clone()))
                    .or_default();
                agg.runs += 1;
                if run.ts > agg.last {
                    agg.last = run.ts.clone();
                }
                if let Some(obj) = run.arguments.as_object() {
                    for k in obj.keys() {
                        *agg.arg_keys.entry(k.clone()).or_default() += 1;
                    }
                }
            });
            self.files.insert(path.clone(), FileAgg { mtime, size, pairs });
            reparsed += 1;
        }
        (self.files.len(), reparsed)
    }

    /// Files whose aggregates contain at least one (provider, tool) pair
    /// matching the filters — so extraction never opens irrelevant sessions.
    fn matching_files(
        &self,
        providers: &HashSet<String>,
        tools: &Option<globset::GlobSet>,
    ) -> Vec<PathBuf> {
        self.files
            .iter()
            .filter(|(_, agg)| {
                agg.pairs.iter().any(|((p, t), _)| {
                    (providers.is_empty() || providers.contains(p))
                        && tools.as_ref().is_none_or(|g| g.is_match(t))
                })
            })
            .map(|(p, _)| p.clone())
            .collect()
    }
}

// ---------------------------------------------------------------------------
// HTTP server
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct AppState {
    index: Arc<Mutex<Index>>,
    roots: Arc<Vec<PathBuf>>,
}

fn bearer_ok(headers: &axum::http::HeaderMap) -> bool {
    let Ok(expected) = std::env::var("SAILFISH_N8_TOKEN") else {
        return true; // no token configured → open (localhost-only anyway)
    };
    let expected = expected.trim();
    if expected.is_empty() {
        return true;
    }
    headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|t| t.trim() == expected)
}

async fn auth_mw(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    if !bearer_ok(req.headers()) {
        return (StatusCode::UNAUTHORIZED, "bad or missing bearer token").into_response();
    }
    next.run(req).await
}

fn refreshed(state: &AppState) -> std::sync::MutexGuard<'_, Index> {
    let mut idx = state.index.lock().unwrap_or_else(|p| p.into_inner());
    idx.refresh(&state.roots);
    idx
}

// --- GET /v1/providers ------------------------------------------------------

#[derive(Serialize)]
struct ProviderRow {
    id: String,
    label: String,
    tool_runs: u64,
    tools: u64,
    last: String,
}

async fn providers(State(state): State<AppState>) -> Json<serde_json::Value> {
    let state2 = state.clone();
    let rows = tokio::task::spawn_blocking(move || {
        let idx = refreshed(&state2);
        let mut by_provider: HashMap<String, (u64, HashSet<String>, String)> = HashMap::new();
        for agg in idx.files.values() {
            for ((p, t), pa) in &agg.pairs {
                let e = by_provider.entry(p.clone()).or_default();
                e.0 += pa.runs;
                e.1.insert(t.clone());
                if pa.last > e.2 {
                    e.2 = pa.last.clone();
                }
            }
        }
        let mut rows: Vec<ProviderRow> = by_provider
            .into_iter()
            .map(|(id, (runs, tools, last))| ProviderRow {
                label: label_for(&id),
                id,
                tool_runs: runs,
                tools: tools.len() as u64,
                last,
            })
            .collect();
        rows.sort_by(|a, b| b.tool_runs.cmp(&a.tool_runs));
        rows
    })
    .await
    .unwrap_or_default();
    Json(serde_json::json!({ "providers": rows }))
}

fn label_for(id: &str) -> String {
    let mut out = String::new();
    for (i, part) in id.split(['-', '_']).enumerate() {
        if i > 0 {
            out.push(' ');
        }
        let mut cs = part.chars();
        if let Some(c) = cs.next() {
            out.extend(c.to_uppercase());
            out.push_str(cs.as_str());
        }
    }
    out
}

// --- GET /v1/tools/search ----------------------------------------------------

#[derive(Deserialize)]
struct SearchQ {
    q: Option<String>,
    provider: Option<String>,
    tool: Option<String>,
    limit: Option<usize>,
}

async fn tools_search(
    State(state): State<AppState>,
    Query(q): Query<SearchQ>,
) -> Json<serde_json::Value> {
    let state2 = state.clone();
    let needle = q.q.clone().unwrap_or_default().to_lowercase();
    let provider = q.provider.clone();
    let tool = q.tool.clone();
    let limit = q.limit.unwrap_or(100);
    let matches = tokio::task::spawn_blocking(move || {
        let idx = refreshed(&state2);
        let mut by_pair: HashMap<(String, String), u64> = HashMap::new();
        for agg in idx.files.values() {
            for ((p, t), pa) in &agg.pairs {
                *by_pair.entry((p.clone(), t.clone())).or_default() += pa.runs;
            }
        }
        let mut rows: Vec<serde_json::Value> = by_pair
            .into_iter()
            .filter(|((p, t), _)| {
                provider.as_deref().is_none_or(|f| p == f)
                    && tool.as_deref().is_none_or(|f| t == f)
                    && (needle.is_empty() || t.to_lowercase().contains(&needle))
            })
            .map(|((p, t), runs)| {
                serde_json::json!({ "tool": t, "provider": p, "runs": runs, "description": "" })
            })
            .collect();
        rows.sort_by_key(|r| std::cmp::Reverse(r["runs"].as_u64().unwrap_or(0)));
        rows.truncate(limit);
        rows
    })
    .await
    .unwrap_or_default();
    Json(serde_json::json!({
        "query": { "q": q.q, "provider": q.provider, "tool": q.tool },
        "matches": matches
    }))
}

// --- GET /v1/tool-runs (JSONL stream) ----------------------------------------

async fn tool_runs(
    State(state): State<AppState>,
    Query(params): Query<Vec<(String, String)>>,
) -> Response {
    // Repeatable params (provider=…&provider=…) — Query as pair-list keeps them all.
    let mut providers: HashSet<String> = HashSet::new();
    let mut tool_globs: Vec<String> = Vec::new();
    let mut since = String::new();
    let mut limit = usize::MAX;
    let mut format = "jsonl".to_string();
    let mut clean = true;
    for (k, v) in params {
        match k.as_str() {
            "provider" => {
                providers.insert(v);
            }
            "tool" => tool_globs.push(v),
            "since" => since = v,
            "limit" => limit = v.parse().unwrap_or(usize::MAX),
            "format" => format = v,
            "clean" => clean = v != "0",
            _ => {}
        }
    }
    if format == "zip" {
        return (
            StatusCode::NOT_IMPLEMENTED,
            "format=zip not implemented yet — use jsonl (default)",
        )
            .into_response();
    }

    let globs = if tool_globs.is_empty() {
        None
    } else {
        let mut b = globset::GlobSetBuilder::new();
        for g in &tool_globs {
            if let Ok(glob) = globset::Glob::new(g) {
                b.add(glob);
            }
        }
        b.build().ok()
    };

    let files = {
        let state2 = state.clone();
        let providers = providers.clone();
        let globs2 = globs.clone();
        tokio::task::spawn_blocking(move || {
            refreshed(&state2).matching_files(&providers, &globs2)
        })
        .await
        .unwrap_or_default()
    };

    let (tx, rx) = tokio::sync::mpsc::channel::<Result<axum::body::Bytes, std::io::Error>>(64);
    tokio::task::spawn_blocking(move || {
        let mut sent = 0usize;
        let mut seen: HashSet<u64> = HashSet::new();
        'files: for path in files {
            let mut stop = false;
            let _ = extract_file(&path, clean, |run| {
                if stop || sent >= limit {
                    stop = true;
                    return;
                }
                if !providers.is_empty() && !providers.contains(&run.provider) {
                    return;
                }
                if let Some(g) = &globs {
                    if !g.is_match(&run.tool) {
                        return;
                    }
                }
                if !since.is_empty() && run.ts.as_str() < since.as_str() {
                    return;
                }
                // De-dup identical runs (same tool+args+result) across the export.
                use std::hash::{Hash, Hasher};
                let mut h = std::collections::hash_map::DefaultHasher::new();
                run.provider.hash(&mut h);
                run.tool.hash(&mut h);
                run.arguments.to_string().hash(&mut h);
                run.result_preview.hash(&mut h);
                if !seen.insert(h.finish()) {
                    return;
                }
                if let Ok(mut line) = serde_json::to_string(&run) {
                    line.push('\n');
                    if tx.blocking_send(Ok(line.into())).is_err() {
                        stop = true; // client hung up
                    } else {
                        sent += 1;
                    }
                }
            });
            if stop && sent >= limit {
                break 'files;
            }
            if tx.is_closed() {
                break 'files;
            }
        }
    });

    let stream = futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    });
    Response::builder()
        .header("content-type", "application/jsonl")
        .body(axum::body::Body::from_stream(stream))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

// --- GET /v1/stats ------------------------------------------------------------

#[derive(Deserialize)]
struct StatsQ {
    provider: Option<String>,
}

async fn stats(
    State(state): State<AppState>,
    Query(q): Query<StatsQ>,
) -> Json<serde_json::Value> {
    let state2 = state.clone();
    let provider = q.provider.clone();
    let out = tokio::task::spawn_blocking(move || {
        let idx = refreshed(&state2);
        let mut tools: HashMap<(String, String), u64> = HashMap::new();
        let mut arg_keys: HashMap<(String, String), HashMap<String, u64>> = HashMap::new();
        for agg in idx.files.values() {
            for ((p, t), pa) in &agg.pairs {
                if provider.as_deref().is_some_and(|f| p != f) {
                    continue;
                }
                *tools.entry((p.clone(), t.clone())).or_default() += pa.runs;
                let e = arg_keys.entry((p.clone(), t.clone())).or_default();
                for (k, n) in &pa.arg_keys {
                    *e.entry(k.clone()).or_default() += n;
                }
            }
        }
        let mut tool_rows: Vec<serde_json::Value> = tools
            .iter()
            .map(|((p, t), n)| serde_json::json!({ "provider": p, "tool": t, "runs": n }))
            .collect();
        tool_rows.sort_by_key(|r| std::cmp::Reverse(r["runs"].as_u64().unwrap_or(0)));
        let arg_rows: Vec<serde_json::Value> = arg_keys
            .into_iter()
            .map(|((p, t), keys)| {
                serde_json::json!({ "provider": p, "tool": t, "arg_keys": keys })
            })
            .collect();
        serde_json::json!({ "tools": tool_rows, "arg_keys": arg_rows })
    })
    .await
    .unwrap_or_else(|_| serde_json::json!({}));
    Json(out)
}

// --- appliance lifecycle (§1.5) ------------------------------------------------

const APPLIANCE_NAME: &str = "sailfish";
const APPLIANCE_IMAGE: &str = "deepbluedynamics/sailfish:latest";

fn docker(args: &[&str]) -> (bool, String) {
    match std::process::Command::new("docker").args(args).output() {
        Ok(o) => (
            o.status.success(),
            format!(
                "{}{}",
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr)
            ),
        ),
        Err(e) => (false, e.to_string()),
    }
}

async fn appliance_status() -> Json<serde_json::Value> {
    let out = tokio::task::spawn_blocking(|| {
        let (ok, txt) = docker(&[
            "inspect",
            "--format",
            "{{.State.Status}}",
            APPLIANCE_NAME,
        ]);
        let status = txt.trim().to_string();
        serde_json::json!({
            "installed": ok,
            "running": ok && status == "running",
            "status": if ok { status } else { "absent".to_string() },
        })
    })
    .await
    .unwrap_or_else(|_| serde_json::json!({"installed": false, "running": false}));
    Json(out)
}

async fn appliance_ensure() -> Json<serde_json::Value> {
    let out = tokio::task::spawn_blocking(|| {
        // Existing container → just start it (idempotent on running).
        let (exists, _) = docker(&["inspect", APPLIANCE_NAME]);
        if exists {
            let (ok, txt) = docker(&["start", APPLIANCE_NAME]);
            return serde_json::json!({ "action": "start", "ok": ok, "detail": txt.trim() });
        }
        let claude = dirs::home_dir()
            .unwrap_or_default()
            .join(".claude")
            .display()
            .to_string();
        let vol = format!("{claude}:/root/.claude:ro");
        // GPU first; retry CPU-only for boxes without --gpus support.
        let base = [
            "run", "-d", "--name", APPLIANCE_NAME, "-p", "22343:22343", "-v", &vol,
        ];
        let mut with_gpu: Vec<&str> = base.to_vec();
        with_gpu.extend(["--gpus", "all", APPLIANCE_IMAGE]);
        let (ok, txt) = docker(&with_gpu);
        if ok {
            return serde_json::json!({ "action": "run", "gpus": true, "ok": true, "detail": txt.trim() });
        }
        let mut no_gpu: Vec<&str> = base.to_vec();
        no_gpu.push(APPLIANCE_IMAGE);
        let (ok2, txt2) = docker(&no_gpu);
        serde_json::json!({ "action": "run", "gpus": false, "ok": ok2, "detail": format!("{} | {}", txt.trim(), txt2.trim()) })
    })
    .await
    .unwrap_or_else(|_| serde_json::json!({"ok": false}));
    Json(out)
}

// --- server -------------------------------------------------------------------

pub async fn serve(port: u16) -> anyhow::Result<()> {
    let state = AppState {
        index: Arc::new(Mutex::new(Index::default())),
        roots: Arc::new(source_roots()),
    };

    // Prime the index before accepting traffic (first parse is the big one).
    {
        let state2 = state.clone();
        let (files, parsed) = tokio::task::spawn_blocking(move || {
            let mut idx = state2.index.lock().unwrap_or_else(|p| p.into_inner());
            idx.refresh(&state2.roots)
        })
        .await?;
        eprintln!("[trainer-api] indexed {files} session files ({parsed} parsed)");
    }

    let app = Router::new()
        .route("/v1/providers", get(providers))
        .route("/v1/tools/search", get(tools_search))
        .route("/v1/tool-runs", get(tool_runs))
        .route("/v1/stats", get(stats))
        .route("/v1/appliance", get(appliance_status))
        .route("/v1/appliance/ensure", post(appliance_ensure))
        .layer(axum::middleware::from_fn(auth_mw))
        .with_state(state);

    // Localhost ONLY — training data never binds an external interface.
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    eprintln!("[trainer-api] listening on http://127.0.0.1:{port} (localhost-only)");
    axum::serve(listener, app).await?;
    Ok(())
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn scrub_strips_ansi_and_controls() {
        assert_eq!(scrub("\u{1b}[31mred\u{1b}[0m ok"), "red ok");
        assert_eq!(scrub("a\r\nb\0c"), "a\nbc");
        assert_eq!(scrub("\u{1b}]0;title\u{7}text"), "text");
        assert_eq!(scrub("tab\tkept"), "tab\tkept");
    }

    #[test]
    fn split_tool_providers() {
        assert_eq!(
            split_tool("mcp__hyperia__terminal_run"),
            ("hyperia".into(), "terminal_run".into())
        );
        assert_eq!(split_tool("Bash"), ("claude-code".into(), "Bash".into()));
    }

    fn mock_session(dir: &Path) -> PathBuf {
        let p = dir.join("session-aabbccdd.jsonl");
        // The tool_result carries a JSON-escaped ANSI color sequence ([32m…)
        // — built via format! so no raw control byte lands in this source file.
        let ansi_result = format!(
            r#"{{"type":"user","timestamp":"2026-07-01T00:00:06Z","message":{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"tu1","content":[{{"type":"text","text":"{e}[32mfile-a file-b{e}[0m"}}]}}]}}}}"#,
            e = "\\u001b"
        );
        let lines = [
            r#"{"type":"user","timestamp":"2026-07-01T00:00:00Z","message":{"role":"user","content":"list the files"}}"#.to_string(),
            r#"{"type":"assistant","timestamp":"2026-07-01T00:00:05Z","message":{"role":"assistant","content":[{"type":"text","text":"Listing now."},{"type":"tool_use","id":"tu1","name":"mcp__hyperia__terminal_run","input":{"command":"ls -la","pane":2}}]}}"#.to_string(),
            ansi_result,
            r#"{"type":"assistant","timestamp":"2026-07-01T00:00:08Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"tu2","name":"Bash","input":{"command":"echo hi"}}]}}"#.to_string(),
        ];
        let mut f = std::fs::File::create(&p).unwrap();
        for l in lines {
            writeln!(f, "{l}").unwrap();
        }
        p
    }

    #[test]
    fn extract_builds_records_with_context_and_preview() {
        let dir = tempfile::tempdir().unwrap();
        let p = mock_session(dir.path());
        let mut runs = Vec::new();
        extract_file(&p, true, |r| runs.push(r)).unwrap();
        assert_eq!(runs.len(), 2);

        let hy = &runs[0];
        assert_eq!(hy.provider, "hyperia");
        assert_eq!(hy.tool, "terminal_run");
        assert_eq!(hy.arguments["command"], "ls -la");
        assert_eq!(hy.result_preview, "file-a file-b"); // ANSI scrubbed
        // context: the user turn + the assistant's own lead-up text
        assert_eq!(hy.context.len(), 2);
        assert_eq!(hy.context[0].role, "user");
        assert_eq!(hy.context[0].text, "list the files");
        assert_eq!(hy.context[1].role, "assistant");

        // tu2 never got a result — emitted at EOF with empty preview
        let bash = &runs[1];
        assert_eq!(bash.provider, "claude-code");
        assert_eq!(bash.tool, "Bash");
        assert_eq!(bash.result_preview, "");
    }

    #[test]
    fn index_refresh_counts_and_is_incremental() {
        let dir = tempfile::tempdir().unwrap();
        let p = mock_session(dir.path());
        let mut idx = Index::default();
        let roots = vec![dir.path().to_path_buf()];
        let (files, parsed) = idx.refresh(&roots);
        assert_eq!((files, parsed), (1, 1));
        // unchanged → nothing reparsed
        let (files, parsed) = idx.refresh(&roots);
        assert_eq!((files, parsed), (1, 0));

        let agg = idx.files.get(&p).unwrap();
        assert_eq!(
            agg.pairs[&("hyperia".to_string(), "terminal_run".to_string())].runs,
            1
        );
        assert_eq!(
            agg.pairs[&("claude-code".to_string(), "Bash".to_string())].runs,
            1
        );
        // arg keys recorded
        assert!(agg.pairs[&("hyperia".to_string(), "terminal_run".to_string())]
            .arg_keys
            .contains_key("command"));
    }

    #[test]
    fn matching_files_filters_by_provider_and_glob() {
        let dir = tempfile::tempdir().unwrap();
        mock_session(dir.path());
        let mut idx = Index::default();
        idx.refresh(&[dir.path().to_path_buf()]);

        let hy: HashSet<String> = ["hyperia".to_string()].into();
        assert_eq!(idx.matching_files(&hy, &None).len(), 1);

        let none: HashSet<String> = ["opencode".to_string()].into();
        assert_eq!(idx.matching_files(&none, &None).len(), 0);

        let mut b = globset::GlobSetBuilder::new();
        b.add(globset::Glob::new("terminal_*").unwrap());
        let g = Some(b.build().unwrap());
        assert_eq!(idx.matching_files(&HashSet::new(), &g).len(), 1);
    }
}
