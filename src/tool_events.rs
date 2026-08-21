//! Tool-call events for the fleet feed — WHO called WHAT with WHICH params.
//!
//! Agents' session transcripts (the JSONL most providers write) carry every
//! `tool_use` block: tool name + full arguments. This tailer follows each
//! running container's resolved session file (the same container→session
//! correlation the fleet's tok/s uses) and synthesizes a `tool_call` event
//! per invocation into the normal event pipeline — ring, lume store, SSE —
//! so the dashboard/kind-filter/search treat them like any other event.
//!
//! Host-side only: no container or monitor changes, works for sessions that
//! are already running.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// How much of a fresh file to replay on first sight, so the feed seeds with
/// RECENT calls instead of starting empty (or flooding with megabytes).
const SEED_BYTES: u64 = 64 * 1024;
/// Argument preview cap in the summary line.
const ARGS_PREVIEW: usize = 220;
/// Full-arguments cap stored on the event (raw JSON string).
const ARGS_FULL_CAP: usize = 4_000;

#[derive(Default)]
pub struct ToolCallTailer {
    offsets: HashMap<PathBuf, u64>,
}

impl ToolCallTailer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Tail newly-appended lines of `session_path`, returning one synthesized
    /// `tool_call` event per tool invocation, parsed with the provider's
    /// declared `dialect` engine, attributed to `agent_id`.
    pub fn poll(
        &mut self,
        session_path: &Path,
        dialect: &str,
        agent_id: &str,
    ) -> Vec<serde_json::Value> {
        let Ok(meta) = std::fs::metadata(session_path) else {
            return Vec::new();
        };
        let size = meta.len();
        let start = match self.offsets.get(session_path).copied() {
            Some(prev) if prev <= size => prev,
            // First sight (or truncation): seed from the recent tail only.
            _ => size.saturating_sub(SEED_BYTES),
        };
        self.offsets.insert(session_path.to_path_buf(), size);
        if size <= start {
            return Vec::new();
        }
        let Ok(chunk) = read_range(session_path, start, size) else {
            return Vec::new();
        };

        let mut out = Vec::new();
        for line in chunk.lines() {
            // If we started mid-file the first line may be partial — a failed
            // JSON parse just skips it.
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
                continue;
            };
            extract_tool_calls(&v, dialect, agent_id, &mut out);
        }
        out
    }
}

/// Pull tool calls out of one transcript line using the provider's declared
/// `dialect` engine (selected by FORMAT, from hooks.tool_dialect): `anthropic`
/// (tool_use content), `openai` (tool_calls[]), `codex` (response_item
/// function_call/custom_tool_call), `antigravity` (brain-transcript actions,
/// reached via tool_transcript_path since agy's own .db is protobuf). Sqlite
/// stores (opencode/hermes) go through SqliteToolTailer instead. Adding a
/// provider that shares a format is pure TOML; a genuinely new format is the
/// only case that needs a new engine here.
fn extract_tool_calls(
    v: &serde_json::Value,
    dialect: &str,
    agent_id: &str,
    out: &mut Vec<serde_json::Value>,
) {
    let ts = v
        .get("timestamp")
        .or_else(|| v.get("created_at")) // antigravity brain transcript
        .and_then(|t| t.as_str())
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.timestamp() as u64)
        .unwrap_or(0);

    match dialect {
        // ── anthropic: assistant message with `tool_use` content blocks
        //    (structured `input`). claude, qwen, gemini-family. ──
        "anthropic" => {
            if v.get("type").and_then(|t| t.as_str()) != Some("assistant") {
                return;
            }
            if let Some(items) = v
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_array())
            {
                for item in items {
                    if item.get("type").and_then(|t| t.as_str()) != Some("tool_use") {
                        continue;
                    }
                    if let Some(name) = item.get("name").and_then(|n| n.as_str()) {
                        out.push(tool_call_event(agent_id, ts, name, &args_of(item)));
                    }
                }
            }
        }
        // ── openai: a `tool_calls` array (name + arguments string), on the
        //    line or under `message`. grok. ──
        "openai" => {
            for holder in [Some(v), v.get("message")].into_iter().flatten() {
                if let Some(calls) = holder.get("tool_calls").and_then(|c| c.as_array()) {
                    for c in calls {
                        if let Some(name) = c.get("name").and_then(|n| n.as_str()) {
                            out.push(tool_call_event(agent_id, ts, name, &args_of(c)));
                        }
                    }
                }
            }
        }
        // ── codex: response_item whose payload is a function_call (`arguments`
        //    string) or custom_tool_call/local_shell_call (`input`). The
        //    custom_tool_call form is the exec/shell majority. codex, sakana. ──
        "codex" => {
            if v.get("type").and_then(|t| t.as_str()) != Some("response_item") {
                return;
            }
            if let Some(p) = v.get("payload") {
                let is_call = matches!(
                    p.get("type").and_then(|t| t.as_str()),
                    Some("function_call") | Some("custom_tool_call") | Some("local_shell_call")
                );
                if is_call {
                    if let Some(name) = p.get("name").and_then(|n| n.as_str()) {
                        out.push(tool_call_event(agent_id, ts, name, &args_of(p)));
                    }
                }
            }
        }
        // ── antigravity brain transcript: each tool action is a line whose
        //    UPPERCASE `type` is the action; `ToolName` names the MCP tool when
        //    present; `content` is the summary. Non-tool turns excluded. ──
        "antigravity" => {
            if let Some(ty) = v.get("type").and_then(|t| t.as_str()) {
                if is_antigravity_action(ty) {
                    let name = v.get("ToolName").and_then(|n| n.as_str()).unwrap_or(ty);
                    let args = v
                        .get("content")
                        .and_then(|c| c.as_str())
                        .map(|s| s.to_string())
                        .unwrap_or_default();
                    out.push(tool_call_event(agent_id, ts, name, &args));
                }
            }
        }
        _ => {}
    }
}

/// Provider → JSONL tool-call dialect engine, from the declared `tool_dialect`.
/// Empty when the provider has no JSONL tool parsing (sqlite, or unmapped).
pub fn tool_dialect(provider: &str) -> Option<&'static str> {
    use std::sync::OnceLock;
    static MAP: OnceLock<HashMap<String, String>> = OnceLock::new();
    let map = MAP.get_or_init(|| {
        crate::provider_registry::ProviderRegistry::load()
            .all()
            .filter(|d| !d.provider.hooks.tool_dialect.is_empty())
            .map(|d| (d.provider.name.clone(), d.provider.hooks.tool_dialect.clone()))
            .collect()
    });
    map.get(provider).map(|s| match s.as_str() {
        "anthropic" => "anthropic",
        "openai" => "openai",
        "antigravity" => "antigravity",
        _ => "codex",
    })
}

/// True for antigravity brain-transcript line types that are tool actions
/// (not model reasoning or user input). Allowlist so a new non-tool type
/// can't false-positive; extend as agy adds actions.
fn is_antigravity_action(ty: &str) -> bool {
    matches!(
        ty,
        "MCP_TOOL"
            | "VIEW_FILE"
            | "EDIT_FILE"
            | "WRITE_FILE"
            | "CREATE_FILE"
            | "DELETE_FILE"
            | "GREP_SEARCH"
            | "CODEBASE_SEARCH"
            | "FILE_SEARCH"
            | "FIND_FILE"
            | "LIST_DIRECTORY"
            | "READ_URL_CONTENT"
            | "RUN_COMMAND"
            | "VIEW_CODE_ITEM"
            | "BROWSER_ACTION"
    )
}

/// Provider → sqlite tool-call reader dialect (opencode / hermes), from the
/// same `session_db_reader` the session listing declares. Some(reader) means
/// this provider's tool calls come from the sqlite `SqliteToolTailer`, not the
/// JSONL `ToolCallTailer`.
pub fn sqlite_tool_reader(provider: &str) -> Option<&'static str> {
    use std::sync::OnceLock;
    static MAP: OnceLock<HashMap<String, String>> = OnceLock::new();
    let map = MAP.get_or_init(|| {
        crate::provider_registry::ProviderRegistry::load()
            .all()
            .filter(|d| !d.provider.hooks.session_db_reader.is_empty())
            .map(|d| (d.provider.name.clone(), d.provider.hooks.session_db_reader.clone()))
            .collect()
    });
    map.get(provider).map(|s| match s.as_str() {
        "hermes" => "hermes",
        _ => "opencode",
    })
}

/// Resolve the path the tailer should poll for a session: the provider's
/// declared `tool_transcript` (relative to the session file's dir, `{id}`
/// substituted) when set, else the session file itself. Registry-driven — no
/// provider layouts live in this file.
pub fn tool_transcript_path(provider: &str, session_path: &Path, session_id: &str) -> PathBuf {
    use std::sync::OnceLock;
    static MAP: OnceLock<HashMap<String, String>> = OnceLock::new();
    let map = MAP.get_or_init(|| {
        crate::provider_registry::ProviderRegistry::load()
            .all()
            .filter(|d| !d.provider.hooks.tool_transcript.is_empty())
            .map(|d| (d.provider.name.clone(), d.provider.hooks.tool_transcript.clone()))
            .collect()
    });
    match map.get(provider) {
        Some(tmpl) => session_path
            .parent()
            .map(|dir| dir.join(tmpl.replace("{id}", session_id)))
            .unwrap_or_else(|| session_path.to_path_buf()),
        None => session_path.to_path_buf(),
    }
}

/// Best-effort args JSON string from a tool-call node, whichever field it uses:
/// `arguments` (already a string — codex/grok), `input` (object or string —
/// claude/custom_tool_call), else a serialized fallback. Never panics.
fn args_of(node: &serde_json::Value) -> String {
    for key in ["arguments", "input", "args", "params"] {
        if let Some(val) = node.get(key) {
            return match val.as_str() {
                Some(s) => s.to_string(),
                None => serde_json::to_string(val).unwrap_or_default(),
            };
        }
    }
    "{}".to_string()
}

/// Tool-call tailer for providers whose transcript is a SHARED sqlite db
/// (opencode `part`, hermes `messages`) rather than an appendable JSONL file.
/// Polls by rowid cursor per (db, session), so it only reads NEW rows. Opens
/// read-only each poll (cheap; sees WAL writes on the real host path).
#[derive(Default)]
pub struct SqliteToolTailer {
    cursors: HashMap<(PathBuf, String), i64>,
}

/// How many trailing rows to seed on first sight of a session (recent tail,
/// not the whole history).
const SQLITE_SEED_ROWS: i64 = 200;

impl SqliteToolTailer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Emit `tool_call` events for `session_id` in `db_path`, using the
    /// provider's declared `session_db_reader` dialect. Empty on unknown
    /// reader, unopenable db, or no new rows.
    pub fn poll(
        &mut self,
        db_path: &Path,
        reader: &str,
        session_id: &str,
        agent_id: &str,
    ) -> Vec<serde_json::Value> {
        let conn = match rusqlite::Connection::open_with_flags(
            db_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        ) {
            Ok(c) => c,
            Err(_) => return Vec::new(),
        };
        let (table, time_col, payload_col) = match reader {
            "opencode" => ("part", "time_created", "data"),
            "hermes" => ("messages", "timestamp", "tool_calls"),
            _ => return Vec::new(),
        };
        let key = (db_path.to_path_buf(), session_id.to_string());
        let start = match self.cursors.get(&key).copied() {
            Some(c) => c,
            None => {
                // First sight: seed from the recent tail (max rowid − N).
                let max: i64 = conn
                    .query_row(
                        &format!("SELECT COALESCE(MAX(rowid),0) FROM {table} WHERE session_id=?1"),
                        [session_id],
                        |r| r.get(0),
                    )
                    .unwrap_or(0);
                (max - SQLITE_SEED_ROWS).max(0)
            }
        };

        let sql = format!(
            "SELECT rowid, {time_col}, {payload_col} FROM {table} \
             WHERE session_id=?1 AND rowid>?2 ORDER BY rowid LIMIT 500"
        );
        let mut stmt = match conn.prepare(&sql) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let rows = stmt.query_map(rusqlite::params![session_id, start], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, Option<i64>>(1).unwrap_or(None),
                r.get::<_, Option<String>>(2).unwrap_or(None),
            ))
        });
        let Ok(rows) = rows else { return Vec::new() };

        let mut out = Vec::new();
        let mut newest = start;
        for (rowid, time_ms, payload) in rows.flatten() {
            newest = newest.max(rowid);
            let ts = time_ms.map(|t| (t / 1000).max(0) as u64).unwrap_or(0);
            let Some(payload) = payload else { continue };
            match reader {
                // opencode: `data` is one part; emit only type==tool.
                "opencode" => {
                    if let Ok(j) = serde_json::from_str::<serde_json::Value>(&payload) {
                        if j.get("type").and_then(|t| t.as_str()) == Some("tool") {
                            let name = j.get("tool").and_then(|t| t.as_str()).unwrap_or("tool");
                            let args = j
                                .get("state")
                                .and_then(|s| s.get("input"))
                                .map(|i| serde_json::to_string(i).unwrap_or_default())
                                .unwrap_or_default();
                            out.push(tool_call_event(agent_id, ts, name, &args));
                        }
                    }
                }
                // hermes: `tool_calls` is an OpenAI-style array of {name, arguments}.
                "hermes" => {
                    if let Ok(calls) = serde_json::from_str::<serde_json::Value>(&payload) {
                        if let Some(arr) = calls.as_array() {
                            for c in arr {
                                let name = c
                                    .get("name")
                                    .or_else(|| c.get("function").and_then(|f| f.get("name")))
                                    .and_then(|n| n.as_str());
                                if let Some(name) = name {
                                    out.push(tool_call_event(agent_id, ts, name, &args_of(c)));
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        self.cursors.insert(key, newest);
        out
    }
}

fn tool_call_event(agent_id: &str, ts: u64, name: &str, args_json: &str) -> serde_json::Value {
    let preview = truncate_chars(args_json, ARGS_PREVIEW);
    serde_json::json!({
        "kind": "tool_call",
        "agent_id": agent_id,
        "ts": ts,
        "tool": name,
        "summary": format!("{name} {preview}"),
        "args": truncate_chars(args_json, ARGS_FULL_CAP),
    })
}

fn truncate_chars(s: &str, cap: usize) -> String {
    if s.chars().count() <= cap {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(cap).collect();
        t.push('…');
        t
    }
}

fn read_range(path: &Path, start: u64, end: u64) -> std::io::Result<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path)?;
    f.seek(SeekFrom::Start(start))?;
    let mut buf = vec![0u8; (end - start) as usize];
    f.read_exact(&mut buf)?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn assistant_line(ts: &str, tool: &str, args: &str) -> String {
        format!(
            r#"{{"type":"assistant","timestamp":"{ts}","message":{{"role":"assistant","content":[{{"type":"tool_use","id":"t1","name":"{tool}","input":{args}}}]}}}}"#
        )
    }

    #[test]
    fn emits_tool_calls_with_name_args_and_attribution() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("session.jsonl");
        std::fs::write(&p, "").unwrap();

        let mut tailer = ToolCallTailer::new();
        // First poll on the empty file: seeds offsets, emits nothing.
        assert!(tailer.poll(&p, "anthropic", "n8-noble-otter").is_empty());

        let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        writeln!(
            f,
            "{}",
            assistant_line(
                "2026-07-07T14:00:00Z",
                "mcp__hyperia__terminal_run",
                r#"{"command":"ls -la","pane":"abc"}"#
            )
        )
        .unwrap();
        drop(f);

        let events = tailer.poll(&p, "anthropic", "n8-noble-otter");
        assert_eq!(events.len(), 1);
        let e = &events[0];
        assert_eq!(e["kind"], "tool_call");
        assert_eq!(e["agent_id"], "n8-noble-otter");
        assert_eq!(e["tool"], "mcp__hyperia__terminal_run");
        assert!(e["summary"].as_str().unwrap().contains("ls -la"));
        assert!(e["args"].as_str().unwrap().contains("\"pane\":\"abc\""));
        assert!(e["ts"].as_u64().unwrap() > 0);

        // No re-emission on an unchanged file.
        assert!(tailer.poll(&p, "anthropic", "n8-noble-otter").is_empty());
    }

    #[test]
    fn parses_codex_rollout_function_calls() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("rollout.jsonl");
        std::fs::write(&p, "").unwrap();
        let mut tailer = ToolCallTailer::new();
        tailer.poll(&p, "codex", "n8-keen-crow");

        let line = r#"{"timestamp":"2026-07-07T20:32:43.040Z","type":"response_item","payload":{"type":"function_call","id":"fc_1","name":"exec_command","arguments":"{\"cmd\":\"sed -n '1,260p' PLAN.md\",\"workdir\":\"/workspace\"}","call_id":"call_x"}}"#;
        let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        writeln!(f, "{line}").unwrap();
        drop(f);

        let events = tailer.poll(&p, "codex", "n8-keen-crow");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["tool"], "exec_command");
        assert_eq!(events[0]["agent_id"], "n8-keen-crow");
        assert!(events[0]["summary"].as_str().unwrap().contains("PLAN.md"));
        assert!(events[0]["ts"].as_u64().unwrap() > 0);
    }

    #[test]
    fn parses_grok_openai_tool_calls_array() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("chat_history.jsonl");
        std::fs::write(&p, "").unwrap();
        let mut tailer = ToolCallTailer::new();
        tailer.poll(&p, "openai", "n8-spry-wren");
        // grok: assistant line with a tool_calls array, arguments = JSON string
        let line = r#"{"type":"assistant","content":"scanning","tool_calls":[{"id":"call-1","name":"read_file","arguments":"{\"target_file\":\"/workspace/x.rs\"}"},{"id":"call-2","name":"enter_plan_mode","arguments":"{}"}]}"#;
        let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        writeln!(f, "{line}").unwrap();
        drop(f);
        let events = tailer.poll(&p, "openai", "n8-spry-wren");
        assert_eq!(events.len(), 2, "both grok tool_calls captured");
        assert_eq!(events[0]["tool"], "read_file");
        assert!(events[0]["args"].as_str().unwrap().contains("/workspace/x.rs"));
        assert_eq!(events[1]["tool"], "enter_plan_mode");
    }

    #[test]
    fn parses_codex_custom_tool_call() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("rollout.jsonl");
        std::fs::write(&p, "").unwrap();
        let mut tailer = ToolCallTailer::new();
        tailer.poll(&p, "codex", "n8-nimble-eel");
        // codex custom_tool_call — the exec-command shape (input, not arguments)
        let line = r#"{"timestamp":"2026-08-20T14:05:24.542Z","type":"response_item","payload":{"type":"custom_tool_call","name":"exec","input":"{\"command\":\"cargo build\"}","call_id":"call_e2"}}"#;
        let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        writeln!(f, "{line}").unwrap();
        drop(f);
        let events = tailer.poll(&p, "codex", "n8-nimble-eel");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["tool"], "exec");
        assert!(events[0]["summary"].as_str().unwrap().contains("cargo build"));
    }

    #[test]
    fn first_sight_seeds_only_the_recent_tail() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("big.jsonl");
        let mut body = String::new();
        for i in 0..3000 {
            body.push_str(&assistant_line(
                "2026-07-07T14:00:00Z",
                "Bash",
                &format!(r#"{{"command":"echo {i}"}}"#),
            ));
            body.push('\n');
        }
        std::fs::write(&p, &body).unwrap();
        let mut tailer = ToolCallTailer::new();
        let events = tailer.poll(&p, "anthropic", "a1");
        // Seeded from the last 64KB — some events, not all 3000.
        assert!(!events.is_empty());
        assert!(events.len() < 3000);
    }
}

#[cfg(test)]
mod antigravity_tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn parses_antigravity_brain_transcript_actions() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("transcript.jsonl");
        std::fs::write(&p, "").unwrap();
        let mut tailer = ToolCallTailer::new();
        tailer.poll(&p, "antigravity", "n8-vivid-crow");
        let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        // a tool action, an MCP tool with ToolName, and a NON-tool turn
        writeln!(f, r#"{{"step_index":8,"source":"MODEL","type":"GREP_SEARCH","created_at":"2026-08-20T12:24:19Z","content":"pattern: fn main"}}"#).unwrap();
        writeln!(f, r#"{{"step_index":6,"source":"MODEL","type":"MCP_TOOL","ToolName":"nuts_edit","created_at":"2026-08-20T12:24:20Z","content":"edited x.rs"}}"#).unwrap();
        writeln!(f, r#"{{"step_index":9,"source":"MODEL","type":"PLANNER_RESPONSE","created_at":"2026-08-20T12:24:21Z","content":"I'll now..."}}"#).unwrap();
        drop(f);
        let ev = tailer.poll(&p, "antigravity", "n8-vivid-crow");
        assert_eq!(ev.len(), 2, "two tool actions, PLANNER_RESPONSE excluded");
        assert_eq!(ev[0]["tool"], "GREP_SEARCH");
        assert_eq!(ev[1]["tool"], "nuts_edit"); // ToolName wins over type
        assert!(ev[0]["ts"].as_u64().unwrap() > 0, "created_at parsed as ts");
    }
}
