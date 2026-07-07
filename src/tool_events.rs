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
    /// `tool_call` event per `tool_use` block, attributed to `agent_id`.
    pub fn poll(
        &mut self,
        session_path: &Path,
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
            if v.get("type").and_then(|t| t.as_str()) != Some("assistant") {
                continue;
            }
            let ts = v
                .get("timestamp")
                .and_then(|t| t.as_str())
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .map(|dt| dt.timestamp() as u64)
                .unwrap_or(0);
            let Some(items) = v
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_array())
            else {
                continue;
            };
            for item in items {
                if item.get("type").and_then(|t| t.as_str()) != Some("tool_use") {
                    continue;
                }
                let Some(name) = item.get("name").and_then(|n| n.as_str()) else {
                    continue;
                };
                let input = item.get("input").cloned().unwrap_or(serde_json::json!({}));
                let args_json = serde_json::to_string(&input).unwrap_or_default();
                let args_full = truncate_chars(&args_json, ARGS_FULL_CAP);
                let preview = truncate_chars(&args_json, ARGS_PREVIEW);
                out.push(serde_json::json!({
                    "kind": "tool_call",
                    "agent_id": agent_id,
                    "ts": ts,
                    "tool": name,
                    "summary": format!("{name} {preview}"),
                    "args": args_full,
                }));
            }
        }
        out
    }
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
        assert!(tailer.poll(&p, "n8-noble-otter").is_empty());

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

        let events = tailer.poll(&p, "n8-noble-otter");
        assert_eq!(events.len(), 1);
        let e = &events[0];
        assert_eq!(e["kind"], "tool_call");
        assert_eq!(e["agent_id"], "n8-noble-otter");
        assert_eq!(e["tool"], "mcp__hyperia__terminal_run");
        assert!(e["summary"].as_str().unwrap().contains("ls -la"));
        assert!(e["args"].as_str().unwrap().contains("\"pane\":\"abc\""));
        assert!(e["ts"].as_u64().unwrap() > 0);

        // No re-emission on an unchanged file.
        assert!(tailer.poll(&p, "n8-noble-otter").is_empty());
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
        let events = tailer.poll(&p, "a1");
        // Seeded from the last 64KB — some events, not all 3000.
        assert!(!events.is_empty());
        assert!(events.len() < 3000);
    }
}
