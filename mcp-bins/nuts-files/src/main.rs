//! nuts-files — one MCP server for all agent file work in nemesis8.
//!
//! Replaces the gnosis-files-{basic,search,diff} + gnosis-code-scan Python
//! tools with a single self-contained Rust binary. Speaks the MCP stdio
//! transport (newline-delimited JSON-RPC 2.0) directly — no SDK, no Python.
//!
//! The headline tool is `nuts_edit`: grapheme-safe, transactional, multi-region
//! file editing backed by aegis-edit (the same LOPT/BFTP engine Hyperia's
//! sidecar uses). Edits validate up front and apply back-to-front, so a bad
//! batch leaves the file untouched and edits never split a codepoint.

use serde_json::{json, Value};
use std::cell::RefCell;
use std::collections::HashMap;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const SERVER_NAME: &str = "nuts-files";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");
const PROTOCOL_VERSION: &str = "2025-06-18";

// ── Request loop ────────────────────────────────────────────────────────────
//
// Every `tools/call` runs on its own thread and responses go out in whatever
// order they finish (JSON-RPC allows that). The loop used to be strictly
// sequential: one long tree walk (a `nuts_search` over a big workspace) blocked
// every later request, and a client that cancelled or timed out that call left
// the walk running with all subsequent `nuts_read`s queued behind it — one
// cancelled search wedged the whole file service. Cancellation is honoured:
// `notifications/cancelled` flags the in-flight request, the walk stops at its
// next entry, and (per MCP) no response is sent for it.

thread_local! {
    /// The cancel flag of the tools/call this worker thread is serving.
    static CANCEL: RefCell<Option<Arc<AtomicBool>>> = const { RefCell::new(None) };
}

/// Has the request this thread is serving been cancelled?
fn cancelled() -> bool {
    CANCEL.with(|c| c.borrow().as_ref().map(|f| f.load(Ordering::Relaxed)).unwrap_or(false))
}

/// One JSON-RPC message per line, atomically (threads share stdout).
fn write_line(out: &Mutex<io::Stdout>, v: Value) {
    if let Ok(mut o) = out.lock() {
        let _ = writeln!(o, "{v}");
        let _ = o.flush();
    }
}

fn main() {
    let stdin = io::stdin();
    let out: Arc<Mutex<io::Stdout>> = Arc::new(Mutex::new(io::stdout()));
    // In-flight tools/call requests by id (as JSON text, so 1 and "1" differ
    // exactly as they do on the wire), for `notifications/cancelled`.
    let inflight: Arc<Mutex<HashMap<String, Arc<AtomicBool>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let req: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue, // ignore non-JSON noise
        };
        let id = req.get("id").cloned();
        let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");

        match method {
            "initialize" => {
                // Echo the client's requested protocolVersion (proper MCP
                // negotiation). Hard-coding our own version makes strict
                // clients (e.g. antigravity/agy) list the tools but refuse to
                // route tools/call — the "lists but never calls" failure.
                let client_pv = req
                    .get("params")
                    .and_then(|p| p.get("protocolVersion"))
                    .and_then(|v| v.as_str())
                    .unwrap_or(PROTOCOL_VERSION);
                write_line(
                    &out,
                    ok(
                        id,
                        json!({
                            "protocolVersion": client_pv,
                            "capabilities": { "tools": {} },
                            "serverInfo": { "name": SERVER_NAME, "version": SERVER_VERSION },
                        }),
                    ),
                );
            }
            "tools/list" => write_line(&out, ok(id, json!({ "tools": tool_list() }))),
            "ping" => write_line(&out, ok(id, json!({}))),
            "notifications/cancelled" => {
                if let Some(rid) = req.get("params").and_then(|p| p.get("requestId")) {
                    if let Ok(map) = inflight.lock() {
                        if let Some(flag) = map.get(&rid.to_string()) {
                            flag.store(true, Ordering::Relaxed);
                        }
                    }
                }
            }
            "tools/call" => {
                let cancel = Arc::new(AtomicBool::new(false));
                let key = id.as_ref().map(|v| v.to_string());
                if let (Some(k), Ok(mut map)) = (&key, inflight.lock()) {
                    map.insert(k.clone(), cancel.clone());
                }
                let params = req.get("params").cloned();
                let out = out.clone();
                let inflight = inflight.clone();
                std::thread::spawn(move || {
                    CANCEL.with(|c| *c.borrow_mut() = Some(cancel.clone()));
                    let resp = handle_call(id, params.as_ref());
                    if let (Some(k), Ok(mut map)) = (&key, inflight.lock()) {
                        map.remove(k);
                    }
                    // A cancelled request gets no response (MCP: the receiver
                    // SHOULD NOT reply; the client has stopped waiting anyway).
                    if !cancel.load(Ordering::Relaxed) {
                        write_line(&out, resp);
                    }
                });
            }
            _ if id.is_some() => {
                write_line(&out, err(id, -32601, &format!("method not found: {method}")))
            }
            _ => {} // other notifications (e.g. notifications/initialized)
        }
    }
}

fn ok(id: Option<Value>, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}
fn err(id: Option<Value>, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// Wrap a tool's Result into the MCP tools/call response shape.
fn handle_call(id: Option<Value>, params: Option<&Value>) -> Value {
    let name = params
        .and_then(|p| p.get("name"))
        .and_then(|n| n.as_str())
        .unwrap_or("");
    let args = params
        .and_then(|p| p.get("arguments"))
        .cloned()
        .unwrap_or(json!({}));
    match dispatch(name, &args) {
        Ok(text) => ok(
            id,
            json!({ "content": [{ "type": "text", "text": text }] }),
        ),
        Err(e) => ok(
            id,
            json!({ "content": [{ "type": "text", "text": format!("error: {e}") }], "isError": true }),
        ),
    }
}

fn dispatch(name: &str, a: &Value) -> Result<String, String> {
    match name {
        "nuts_read" => nuts_read(a),
        "nuts_write" => nuts_write(a),
        "nuts_edit" => nuts_edit(a),
        "nuts_replace" => nuts_replace(a),
        "nuts_stat" => nuts_stat(a),
        "nuts_list" => nuts_list(a),
        "nuts_find" => nuts_find(a),
        "nuts_search" => nuts_search(a),
        "nuts_tree" => nuts_tree(a),
        "nuts_diff" => nuts_diff(a),
        "nuts_delete" => nuts_delete(a),
        "nuts_copy_move" => nuts_copy_move(a),
        _ => Err(format!("unknown tool: {name}")),
    }
}

// ── arg helpers ───────────────────────────────────────────────────────────────

fn sreq(a: &Value, k: &str) -> Result<String, String> {
    a.get(k)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| format!("missing required string '{k}'"))
}
fn sopt(a: &Value, k: &str, default: &str) -> String {
    a.get(k).and_then(|v| v.as_str()).unwrap_or(default).to_string()
}
fn bopt(a: &Value, k: &str, default: bool) -> bool {
    a.get(k).and_then(|v| v.as_bool()).unwrap_or(default)
}
fn uopt(a: &Value, k: &str, default: u64) -> u64 {
    a.get(k).and_then(|v| v.as_u64()).unwrap_or(default)
}

/// Write a file atomically (temp in the same dir + rename).
fn atomic_write(path: &Path, content: &str) -> Result<(), String> {
    let tmp = path.with_extension(format!("nutstmp{}", std::process::id()));
    std::fs::write(&tmp, content.as_bytes()).map_err(|e| format!("write tmp: {e}"))?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("rename: {e}")
    })
}

// ── tools ──────────────────────────────────────────────────────────────────────

fn nuts_read(a: &Value) -> Result<String, String> {
    let path = sreq(a, "path")?;
    std::fs::read_to_string(&path).map_err(|e| format!("read {path}: {e}"))
}

fn nuts_write(a: &Value) -> Result<String, String> {
    let path = sreq(a, "path")?;
    let content = sreq(a, "content")?;
    let p = PathBuf::from(&path);
    if bopt(a, "create_dirs", true) {
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).ok();
        }
    }
    atomic_write(&p, &content)?;
    Ok(format!("wrote {} bytes to {path}", content.len()))
}

fn nuts_edit(a: &Value) -> Result<String, String> {
    use aegis_edit::{Document, TextEdit};
    let path = sreq(a, "path")?;
    let preview = bopt(a, "preview", false);
    let arr = a
        .get("edits")
        .and_then(|v| v.as_array())
        .ok_or("'edits' must be a non-empty array")?;
    if arr.is_empty() {
        return Err("'edits' must be a non-empty array".into());
    }
    let edits: Vec<TextEdit> = arr
        .iter()
        .map(|e| TextEdit {
            start_line: e["start_line"].as_u64().unwrap_or(0) as usize,
            start_col: e["start_col"].as_u64().unwrap_or(0) as usize,
            end_line: e["end_line"].as_u64().unwrap_or(0) as usize,
            end_col: e["end_col"].as_u64().unwrap_or(0) as usize,
            text: e["text"].as_str().unwrap_or("").to_string(),
        })
        .collect();
    let n = edits.len();
    let content = std::fs::read_to_string(&path).map_err(|e| format!("read {path}: {e}"))?;
    let mut doc = Document::new(content);
    // Validates + applies back-to-front; on any error the file is untouched.
    doc.apply_transactional_edits(edits)?;
    let new_content = doc.render();
    if !preview {
        atomic_write(&PathBuf::from(&path), &new_content)?;
    }
    let head: String = new_content.chars().take(2000).collect();
    Ok(format!(
        "{} {n} edit(s) to {path} ({} lines)\n---\n{head}{}",
        if preview { "PREVIEW (not written):" } else { "applied" },
        doc.line_count(),
        if new_content.chars().count() > 2000 { "\n…[truncated]" } else { "" }
    ))
}

fn nuts_replace(a: &Value) -> Result<String, String> {
    let path = sreq(a, "path")?;
    let search = sreq(a, "search")?;
    let replace = sreq(a, "replace")?;
    let preview = bopt(a, "preview", false);
    let max = uopt(a, "max_replacements", 0) as usize; // 0 = all
    let content = std::fs::read_to_string(&path).map_err(|e| format!("read {path}: {e}"))?;
    let count = content.matches(&search).count();
    if count == 0 {
        return Err(format!("search text not found in {path}"));
    }
    let new_content = if max == 0 {
        content.replace(&search, &replace)
    } else {
        content.replacen(&search, &replace, max)
    };
    let applied = if max == 0 { count } else { count.min(max) };
    if !preview {
        atomic_write(&PathBuf::from(&path), &new_content)?;
    }
    Ok(format!(
        "{} {applied} replacement(s) in {path}",
        if preview { "PREVIEW:" } else { "made" }
    ))
}

fn nuts_stat(a: &Value) -> Result<String, String> {
    let path = sreq(a, "path")?;
    let m = std::fs::metadata(&path).map_err(|e| format!("stat {path}: {e}"))?;
    let kind = if m.is_dir() { "dir" } else if m.is_file() { "file" } else { "other" };
    let modified = m
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    Ok(json!({ "path": path, "type": kind, "size_bytes": m.len(), "modified_unix": modified, "readonly": m.permissions().readonly() }).to_string())
}

fn nuts_delete(a: &Value) -> Result<String, String> {
    let path = sreq(a, "path")?;
    let p = PathBuf::from(&path);
    if !p.exists() {
        return Err(format!("not found: {path}"));
    }
    if p.is_dir() {
        if bopt(a, "recursive", false) {
            std::fs::remove_dir_all(&p).map_err(|e| e.to_string())?;
        } else {
            std::fs::remove_dir(&p).map_err(|e| format!("{e} (set recursive=true for non-empty dirs)"))?;
        }
    } else {
        std::fs::remove_file(&p).map_err(|e| e.to_string())?;
    }
    Ok(format!("deleted {path}"))
}

fn nuts_copy_move(a: &Value) -> Result<String, String> {
    let src = sreq(a, "source")?;
    let dst = sreq(a, "destination")?;
    let do_move = bopt(a, "move", false);
    if PathBuf::from(&dst).exists() && !bopt(a, "overwrite", false) {
        return Err(format!("destination exists (set overwrite=true): {dst}"));
    }
    if let Some(parent) = PathBuf::from(&dst).parent() {
        std::fs::create_dir_all(parent).ok();
    }
    if do_move {
        std::fs::rename(&src, &dst).map_err(|e| e.to_string())?;
        Ok(format!("moved {src} -> {dst}"))
    } else {
        std::fs::copy(&src, &dst).map_err(|e| e.to_string())?;
        Ok(format!("copied {src} -> {dst}"))
    }
}

fn nuts_list(a: &Value) -> Result<String, String> {
    let dir = sreq(a, "directory")?;
    let include_hidden = bopt(a, "include_hidden", false);
    let mut out = Vec::new();
    let rd = std::fs::read_dir(&dir).map_err(|e| format!("read_dir {dir}: {e}"))?;
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if !include_hidden && name.starts_with('.') {
            continue;
        }
        let is_dir = e.path().is_dir();
        out.push(json!({ "name": name, "type": if is_dir { "dir" } else { "file" } }));
    }
    out.sort_by(|x, y| x["name"].as_str().cmp(&y["name"].as_str()));
    Ok(json!({ "directory": dir, "entries": out }).to_string())
}

/// Hard limits on any single tree walk. A search over a big workspace used to
/// run to completion no matter what — after the client had cancelled, and long
/// after it had all the results it asked for — and everything queued behind it.
const WALK_MAX_ENTRIES: usize = 50_000;
const WALK_MAX_TIME: Duration = Duration::from_secs(20);

/// Budget for one walk: entry cap, wall-clock deadline, and the request's cancel
/// flag. `truncated` says which one stopped the walk early, so results can say so.
struct WalkBudget {
    entries_left: usize,
    deadline: Instant,
    truncated: Option<&'static str>,
}

impl WalkBudget {
    fn new(max_entries: usize, max_time: Duration) -> Self {
        Self { entries_left: max_entries, deadline: Instant::now() + max_time, truncated: None }
    }

    fn default_limits() -> Self {
        Self::new(WALK_MAX_ENTRIES, WALK_MAX_TIME)
    }

    /// Account for one entry; false = stop the walk (and remember why).
    fn tick(&mut self) -> bool {
        if cancelled() {
            self.truncated = Some("cancelled");
            return false;
        }
        if self.entries_left == 0 {
            self.truncated = Some("entry cap");
            return false;
        }
        if Instant::now() >= self.deadline {
            self.truncated = Some("time cap");
            return false;
        }
        self.entries_left -= 1;
        true
    }
}

/// Recursively walk a dir, calling `f(path, depth)` for each entry until `f`
/// returns false (the caller has what it needs) or the budget runs out. Skips
/// hidden + common heavy dirs unless include_hidden. Bounded by max_depth.
/// Returns false when the walk was stopped early.
fn walk(
    dir: &Path,
    depth: usize,
    max_depth: usize,
    include_hidden: bool,
    budget: &mut WalkBudget,
    f: &mut dyn FnMut(&Path, usize) -> bool,
) -> bool {
    if depth > max_depth {
        return true;
    }
    let Ok(rd) = std::fs::read_dir(dir) else { return true };
    let mut entries: Vec<_> = rd.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        if !budget.tick() {
            return false;
        }
        let name = e.file_name().to_string_lossy().to_string();
        if !include_hidden && name.starts_with('.') {
            continue;
        }
        if matches!(
            name.as_str(),
            "node_modules" | "target" | "__pycache__" | ".git" | "dist" | "build" | "venv" | "vendor"
        ) {
            continue;
        }
        let p = e.path();
        if !f(&p, depth) {
            return false;
        }
        if p.is_dir() && !walk(&p, depth + 1, max_depth, include_hidden, budget, f) {
            return false;
        }
    }
    true
}

fn nuts_find(a: &Value) -> Result<String, String> {
    let dir = sreq(a, "directory")?;
    let pat = sreq(a, "name_pattern")?.to_lowercase();
    let max = uopt(a, "max_results", 200) as usize;
    let mut hits = Vec::new();
    let mut budget = WalkBudget::default_limits();
    walk(Path::new(&dir), 0, 64, bopt(a, "include_hidden", false), &mut budget, &mut |p, _| {
        let name = p.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
        // glob-ish: support a single '*' as a wildcard, else substring
        let m = if let Some((pre, suf)) = pat.split_once('*') {
            name.starts_with(pre) && name.ends_with(suf)
        } else {
            name.contains(&pat)
        };
        if m {
            hits.push(p.to_string_lossy().to_string());
        }
        hits.len() < max // stop walking once we have enough
    });
    let mut out = json!({ "directory": dir, "matches": hits });
    if let Some(why) = budget.truncated {
        out["truncated"] = json!(why);
    }
    Ok(out.to_string())
}

fn nuts_search(a: &Value) -> Result<String, String> {
    let dir = sreq(a, "directory")?;
    let needle = sreq(a, "query")?;
    let needle_l = needle.to_lowercase();
    let file_pat = sopt(a, "file_pattern", "");
    let max = uopt(a, "max_results", 200) as usize;
    let case_sensitive = bopt(a, "case_sensitive", false);
    let mut hits = Vec::new();
    let mut budget = WalkBudget::default_limits();
    walk(Path::new(&dir), 0, 64, bopt(a, "include_hidden", false), &mut budget, &mut |p, _| {
        if !p.is_file() {
            return true;
        }
        if !file_pat.is_empty() {
            let name = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            let ok = if let Some((pre, suf)) = file_pat.split_once('*') {
                name.starts_with(pre) && name.ends_with(suf)
            } else {
                name.contains(&file_pat)
            };
            if !ok {
                return true;
            }
        }
        // Skip files >2MB to stay fast.
        if std::fs::metadata(p).map(|m| m.len() > 2_000_000).unwrap_or(true) {
            return true;
        }
        let Ok(content) = std::fs::read_to_string(p) else { return true };
        for (i, ln) in content.lines().enumerate() {
            let found = if case_sensitive { ln.contains(&needle) } else { ln.to_lowercase().contains(&needle_l) };
            if found {
                hits.push(json!({ "file": p.to_string_lossy(), "line": i + 1, "text": ln.trim() }));
                if hits.len() >= max {
                    break;
                }
            }
        }
        hits.len() < max // stop walking once we have enough
    });
    let mut out = json!({ "query": needle, "matches": hits });
    if let Some(why) = budget.truncated {
        out["truncated"] = json!(why);
    }
    Ok(out.to_string())
}

fn nuts_tree(a: &Value) -> Result<String, String> {
    let dir = sreq(a, "directory")?;
    let max_depth = uopt(a, "max_depth", 4) as usize;
    let include_hidden = bopt(a, "include_hidden", false);
    let mut lines = vec![dir.clone()];
    let mut budget = WalkBudget::default_limits();
    walk(Path::new(&dir), 0, max_depth, include_hidden, &mut budget, &mut |p, depth| {
        let name = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let slash = if p.is_dir() { "/" } else { "" };
        lines.push(format!("{}{}{}", "  ".repeat(depth + 1), name, slash));
        true
    });
    if let Some(why) = budget.truncated {
        lines.push(format!("… (truncated: {why})"));
    }
    Ok(lines.join("\n"))
}

fn nuts_diff(a: &Value) -> Result<String, String> {
    let f1 = sreq(a, "file1")?;
    let f2 = sreq(a, "file2")?;
    let a1 = std::fs::read_to_string(&f1).map_err(|e| format!("read {f1}: {e}"))?;
    let a2 = std::fs::read_to_string(&f2).map_err(|e| format!("read {f2}: {e}"))?;
    let l1: Vec<&str> = a1.lines().collect();
    let l2: Vec<&str> = a2.lines().collect();
    // LCS-based line diff → unified-ish output.
    let n = l1.len();
    let m = l2.len();
    let mut dp = vec![vec![0usize; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i][j] = if l1[i] == l2[j] {
                dp[i + 1][j + 1] + 1
            } else {
                dp[i + 1][j].max(dp[i][j + 1])
            };
        }
    }
    let (mut i, mut j) = (0, 0);
    let mut out = vec![format!("--- {f1}"), format!("+++ {f2}")];
    while i < n && j < m {
        if l1[i] == l2[j] {
            out.push(format!("  {}", l1[i]));
            i += 1;
            j += 1;
        } else if dp[i + 1][j] >= dp[i][j + 1] {
            out.push(format!("- {}", l1[i]));
            i += 1;
        } else {
            out.push(format!("+ {}", l2[j]));
            j += 1;
        }
    }
    while i < n {
        out.push(format!("- {}", l1[i]));
        i += 1;
    }
    while j < m {
        out.push(format!("+ {}", l2[j]));
        j += 1;
    }
    if out.len() == 2 {
        out.push("(files identical)".into());
    }
    Ok(out.join("\n"))
}

// ── tool catalog (schemas + STRONG usage prompting) ─────────────────────────────

fn tool_list() -> Vec<Value> {
    // Descriptions are written to actively steer the agent toward these tools
    // over ad-hoc shell (cat/sed/grep) — clearer intent, safer edits.
    let s = |name: &str, desc: &str, props: Value, required: Vec<&str>| {
        json!({
            "name": name,
            "description": desc,
            "inputSchema": {
                "type": "object",
                "properties": props,
                "required": required,
            }
        })
    };
    let p_str = |d: &str| json!({ "type": "string", "description": d });
    let p_bool = |d: &str| json!({ "type": "boolean", "description": d });
    let p_int = |d: &str| json!({ "type": "integer", "description": d });

    vec![
        s("nuts_edit",
          "PREFERRED way to modify code. Grapheme-safe, transactional, multi-region file edit: \
           supply one or more disjoint {start_line,start_col,end_line,end_col,text} replacements \
           (lines/cols 0-indexed, columns in display characters). All edits validate up front and \
           apply atomically — a bad/overlapping batch leaves the file UNTOUCHED, and edits never \
           corrupt Unicode. Set preview=true to see the result without writing. Use this instead of \
           rewriting whole files or piping through sed.",
          json!({
              "path": p_str("absolute path to the file"),
              "edits": { "type": "array", "description": "disjoint replacements", "items": {
                  "type": "object",
                  "properties": {
                      "start_line": p_int("0-indexed start line"),
                      "start_col": p_int("0-indexed start column (graphemes)"),
                      "end_line": p_int("0-indexed end line"),
                      "end_col": p_int("0-indexed end column (graphemes)"),
                      "text": p_str("replacement text")
                  },
                  "required": ["start_line","start_col","end_line","end_col","text"]
              }},
              "preview": p_bool("if true, return the result without writing (default false)")
          }),
          vec!["path","edits"]),
        s("nuts_replace",
          "Literal search-and-replace in a file — the easy edit when you don't have line/col \
           coordinates. Replaces every occurrence of `search` with `replace` (or the first \
           max_replacements). Errors if `search` isn't found. preview=true to dry-run. Prefer \
           nuts_edit for precise structural edits.",
          json!({
              "path": p_str("absolute path"),
              "search": p_str("exact text to find"),
              "replace": p_str("replacement text"),
              "max_replacements": p_int("0 = all (default)"),
              "preview": p_bool("dry-run (default false)")
          }),
          vec!["path","search","replace"]),
        s("nuts_read", "Read a UTF-8 text file and return its contents. Use this instead of `cat`.",
          json!({ "path": p_str("absolute path") }), vec!["path"]),
        s("nuts_write",
          "Create or overwrite a file with `content` (atomic write; creates parent dirs by default). \
           For changing PART of an existing file, prefer nuts_edit/nuts_replace — don't rewrite the whole thing.",
          json!({ "path": p_str("absolute path"), "content": p_str("full file contents"),
                  "create_dirs": p_bool("mkdir -p parents (default true)") }),
          vec!["path","content"]),
        s("nuts_list", "List a directory's immediate entries. Use instead of `ls`.",
          json!({ "directory": p_str("absolute path"), "include_hidden": p_bool("default false") }),
          vec!["directory"]),
        s("nuts_find", "Find files by name (substring, or a single '*' wildcard) under a directory tree. Use instead of `find -name`.",
          json!({ "directory": p_str("root to search"), "name_pattern": p_str("e.g. 'config' or '*.rs'"),
                  "max_results": p_int("default 200"), "include_hidden": p_bool("default false") }),
          vec!["directory","name_pattern"]),
        s("nuts_search", "Search file CONTENTS for text under a directory tree (returns file:line:text). Use instead of `grep -r`.",
          json!({ "directory": p_str("root"), "query": p_str("text to find"),
                  "file_pattern": p_str("optional name filter, e.g. '*.py'"),
                  "case_sensitive": p_bool("default false"), "max_results": p_int("default 200"),
                  "include_hidden": p_bool("default false") }),
          vec!["directory","query"]),
        s("nuts_tree", "Print a directory tree (skips node_modules/target/.git). Use instead of `tree`.",
          json!({ "directory": p_str("root"), "max_depth": p_int("default 4"), "include_hidden": p_bool("default false") }),
          vec!["directory"]),
        s("nuts_stat", "File/dir metadata: type, size, modified time, readonly.",
          json!({ "path": p_str("absolute path") }), vec!["path"]),
        s("nuts_diff", "Unified line diff between two files.",
          json!({ "file1": p_str("path A"), "file2": p_str("path B") }), vec!["file1","file2"]),
        s("nuts_delete", "Delete a file or directory (recursive=true for non-empty dirs).",
          json!({ "path": p_str("absolute path"), "recursive": p_bool("default false") }), vec!["path"]),
        s("nuts_copy_move", "Copy or move a file (set move=true to move/rename).",
          json!({ "source": p_str("path"), "destination": p_str("path"),
                  "move": p_bool("move instead of copy (default false)"), "overwrite": p_bool("default false") }),
          vec!["source","destination"]),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("nuts-files-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn walk_stops_at_entry_cap() {
        let d = tmp("cap");
        for i in 0..50 {
            std::fs::write(d.join(format!("f{i:02}.txt")), "x").unwrap();
        }
        let mut seen = 0;
        let mut b = WalkBudget::new(10, Duration::from_secs(5));
        let finished = walk(&d, 0, 4, false, &mut b, &mut |_, _| {
            seen += 1;
            true
        });
        assert!(!finished);
        assert_eq!(seen, 10);
        assert_eq!(b.truncated, Some("entry cap"));
    }

    #[test]
    fn walk_honors_the_request_cancel_flag() {
        let d = tmp("cancel");
        for i in 0..20 {
            std::fs::write(d.join(format!("f{i:02}.txt")), "x").unwrap();
        }
        let flag = Arc::new(AtomicBool::new(false));
        CANCEL.with(|c| *c.borrow_mut() = Some(flag.clone()));
        let mut seen = 0;
        let mut b = WalkBudget::new(1000, Duration::from_secs(5));
        walk(&d, 0, 4, false, &mut b, &mut |_, _| {
            seen += 1;
            if seen == 3 {
                flag.store(true, Ordering::Relaxed);
            }
            true
        });
        CANCEL.with(|c| *c.borrow_mut() = None);
        assert_eq!(seen, 3, "the walk stops at the first entry after the cancel");
        assert_eq!(b.truncated, Some("cancelled"));
    }

    #[test]
    fn find_stops_walking_once_it_has_max_results() {
        let d = tmp("find");
        for i in 0..30 {
            std::fs::write(d.join(format!("hit{i:02}.log")), "x").unwrap();
        }
        let out = nuts_find(&json!({
            "directory": d.to_string_lossy(),
            "name_pattern": "hit",
            "max_results": 5
        }))
        .unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["matches"].as_array().unwrap().len(), 5);
        assert!(v.get("truncated").is_none(), "stopping at max_results is not a truncation");
    }

    #[test]
    fn tree_reports_truncation() {
        let d = tmp("tree");
        for i in 0..30 {
            std::fs::write(d.join(format!("f{i:02}.txt")), "x").unwrap();
        }
        let mut b = WalkBudget::new(5, Duration::from_secs(5));
        let mut n = 0;
        walk(&d, 0, 4, false, &mut b, &mut |_, _| {
            n += 1;
            true
        });
        assert_eq!(n, 5);
        assert_eq!(b.truncated, Some("entry cap"));
    }
}
