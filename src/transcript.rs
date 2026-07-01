use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::Path;

#[path = "cost.rs"]
pub mod cost;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolCat {
    Mcp(String),
    Subagent,
    Skill,
    Builtin,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    pub session: String,
    pub agent_id: String,
    pub tool: String,
    pub category: ToolCat,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub ts: String,
    pub model: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionMeta {
    pub id: String,
    pub cwd: String,
    pub model: String,
    pub machine: String,
    pub turns: u64,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub cost_usd: f64,
    pub started: String,
    pub updated: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParsedSession {
    pub meta: SessionMeta,
    pub tool_calls: Vec<ToolCall>,
}

/// Extract session ID from a filename
pub fn extract_session_id(filename: &str) -> Option<String> {
    let stripped = filename
        .trim_end_matches(".jsonl")
        .trim_end_matches(".pb")
        .trim_end_matches(".db");

    // Full UUID at end (Codex format)
    if stripped.len() >= 36 {
        let candidate = &stripped[stripped.len() - 36..];
        if is_uuid_format(candidate) {
            return Some(candidate.to_string());
        }
    }

    // Any filename ending in -<8hex> carries a short ID (provider-agnostic heuristic)
    if let Some((_, short_id)) = stripped.rsplit_once('-') {
        if short_id.len() == 8 && short_id.chars().all(|c| c.is_ascii_hexdigit()) {
            return Some(short_id.to_string());
        }
    }

    // Fallback: use the whole filename stem as the ID
    Some(stripped.to_string())
}

fn is_uuid_format(s: &str) -> bool {
    if s.len() != 36 {
        return false;
    }
    let parts: Vec<&str> = s.split('-').collect();
    if parts.len() != 5 {
        return false;
    }
    let expected_lens = [8, 4, 4, 4, 12];
    for (part, &expected) in parts.iter().zip(&expected_lens) {
        if part.len() != expected || !part.chars().all(|c| c.is_ascii_hexdigit()) {
            return false;
        }
    }
    true
}

/// Classify a tool call name into a ToolCat
pub fn classify_tool(
    name: &str,
    is_subagent_file: bool,
    skill_names: &HashSet<String>,
) -> ToolCat {
    if name.starts_with("mcp__") {
        let parts: Vec<&str> = name.split("__").collect();
        let server = if parts.len() > 1 { parts[1] } else { "" };
        ToolCat::Mcp(server.to_string())
    } else if name == "Task" || is_subagent_file {
        ToolCat::Subagent
    } else if skill_names.contains(name) {
        ToolCat::Skill
    } else {
        ToolCat::Builtin
    }
}

/// Tail-read a file to retrieve at most the last `cap` lines
pub fn read_tail(path: impl AsRef<Path>, cap: usize) -> std::io::Result<Vec<String>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path)?;
    let size = f.metadata()?.len();
    // ~256 B/line heuristic for the read window, clamped to 32 MiB
    let window = (cap as u64).saturating_mul(256).min(32 * 1024 * 1024);
    let start = size.saturating_sub(window);
    f.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    f.read_to_end(&mut bytes)?;
    let text = String::from_utf8_lossy(&bytes);
    // If we began mid-file, the first line is partial — drop it.
    let body: &str = if start > 0 {
        text.find('\n').map(|i| &text[i + 1..]).unwrap_or("")
    } else {
        &text
    };
    let mut lines: Vec<String> = body
        .lines()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if lines.len() > cap {
        let overflow = lines.len() - cap;
        lines.drain(0..overflow);
    }
    Ok(lines)
}

fn parse_line(
    line: &str,
    is_subagent_file: bool,
    session_id: &str,
    agent_id: &str,
    skill_names: &mut HashSet<String>,
    tool_calls: &mut Vec<ToolCall>,
    turns: &mut u64,
    tokens_in: &mut u64,
    tokens_out: &mut u64,
    cost_usd: &mut f64,
    last_cwd: &mut String,
    last_model: &mut String,
    min_ts: &mut Option<String>,
    max_ts: &mut Option<String>,
) {
    let val: serde_json::Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => return,
    };

    // Update timestamps
    if let Some(ts_str) = val.get("timestamp").and_then(|t| t.as_str()) {
        let ts = ts_str.to_string();
        if min_ts.is_none() || ts < *min_ts.as_ref().unwrap() {
            *min_ts = Some(ts.clone());
        }
        if max_ts.is_none() || ts > *max_ts.as_ref().unwrap() {
            *max_ts = Some(ts);
        }
    }

    // Update cwd
    if let Some(cwd_str) = val.get("cwd").and_then(|c| c.as_str()) {
        *last_cwd = cwd_str.to_string();
    }

    // Check type
    let record_type = val.get("type").and_then(|t| t.as_str()).unwrap_or("");

    // If user message, increment turns
    if record_type == "user" {
        *turns += 1;
    }

    // Parse skill listing if present
    if let Some(attachment) = val.get("attachment") {
        if attachment.get("type").and_then(|t| t.as_str()) == Some("skill_listing") {
            if let Some(names) = attachment.get("names").and_then(|n| n.as_array()) {
                for name in names {
                    if let Some(name_str) = name.as_str() {
                        skill_names.insert(name_str.to_string());
                    }
                }
            }
        }
    }

    // Parse assistant message
    if record_type == "assistant" {
        if let Some(message) = val.get("message") {
            // Extract model
            let mut model_str = message
                .get("model")
                .and_then(|m| m.as_str())
                .unwrap_or("")
                .to_string();
            if model_str == "<synthetic>" || model_str.is_empty() {
                // Keep last model if synthetic or empty
            } else {
                *last_model = model_str.clone();
            }
            if model_str.is_empty() && !last_model.is_empty() {
                model_str = last_model.clone();
            }

            // Extract tokens and cost
            let mut msg_in = 0;
            let mut msg_out = 0;
            if let Some(usage) = message.get("usage") {
                msg_in = usage.get("input_tokens").and_then(|i| i.as_u64()).unwrap_or(0);
                msg_out = usage
                    .get("output_tokens")
                    .and_then(|o| o.as_u64())
                    .unwrap_or(0);
            }
            *tokens_in += msg_in;
            *tokens_out += msg_out;
            *cost_usd += cost::cost(msg_in, msg_out, &model_str);

            // Extract tool calls
            if let Some(content) = message.get("content").and_then(|c| c.as_array()) {
                let ts_str = val
                    .get("timestamp")
                    .and_then(|t| t.as_str())
                    .unwrap_or("")
                    .to_string();
                for item in content {
                    if item.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                        if let Some(tool_name) = item.get("name").and_then(|n| n.as_str()) {
                            let category = classify_tool(tool_name, is_subagent_file, skill_names);
                            tool_calls.push(ToolCall {
                                session: session_id.to_string(),
                                agent_id: agent_id.to_string(),
                                tool: tool_name.to_string(),
                                category,
                                tokens_in: msg_in,
                                tokens_out: msg_out,
                                ts: ts_str.clone(),
                                model: model_str.clone(),
                            });
                        }
                    }
                }
            }
        }
    }
}

/// Parse a main session file and its subagent files
pub fn parse_session(main_path: impl AsRef<Path>) -> anyhow::Result<ParsedSession> {
    let main_path = main_path.as_ref();
    let filename = main_path.file_name().and_then(|n| n.to_str()).ok_or_else(|| {
        anyhow::anyhow!("Invalid file path: {}", main_path.display())
    })?;

    let session_id = extract_session_id(filename).unwrap_or_else(|| {
        filename.trim_end_matches(".jsonl").to_string()
    });

    let mut skill_names = HashSet::new();
    let default_skills = [
        "deep-research",
        "update-config",
        "keybindings-help",
        "verify",
        "code-review",
        "simplify",
        "fewer-permission-prompts",
        "loop",
        "schedule",
        "claude-api",
        "run",
        "init",
        "review",
        "security-review",
        "antigravity-guide",
        "explain",
    ];
    for skill in &default_skills {
        skill_names.insert(skill.to_string());
    }

    let mut tool_calls = Vec::new();
    let mut turns = 0;
    let mut tokens_in = 0;
    let mut tokens_out = 0;
    let mut cost_usd = 0.0;
    let mut last_cwd = String::new();
    let mut last_model = String::new();
    let mut min_ts = None;
    let mut max_ts = None;

    // Read and parse main file
    let main_lines = read_tail(main_path, 100_000)?;
    for line in &main_lines {
        parse_line(
            line,
            false,
            &session_id,
            "main",
            &mut skill_names,
            &mut tool_calls,
            &mut turns,
            &mut tokens_in,
            &mut tokens_out,
            &mut cost_usd,
            &mut last_cwd,
            &mut last_model,
            &mut min_ts,
            &mut max_ts,
        );
    }

    // Find and parse subagent files
    if let Some(parent) = main_path.parent() {
        let subagents_dir = parent.join(&session_id).join("subagents");
        if subagents_dir.is_dir() {
            if let Ok(entries) = std::fs::read_dir(subagents_dir) {
                for entry in entries.flatten() {
                    let entry_path = entry.path();
                    if entry_path.is_file() {
                        if let Some(name) = entry_path.file_name().and_then(|n| n.to_str()) {
                            if name.starts_with("agent-") && name.ends_with(".jsonl") {
                                let agent_id = name.trim_end_matches(".jsonl").to_string();
                                let agent_lines = read_tail(&entry_path, 100_000)?;
                                for line in &agent_lines {
                                    parse_line(
                                        line,
                                        true,
                                        &session_id,
                                        &agent_id,
                                        &mut skill_names,
                                        &mut tool_calls,
                                        &mut turns,
                                        &mut tokens_in,
                                        &mut tokens_out,
                                        &mut cost_usd,
                                        &mut last_cwd,
                                        &mut last_model,
                                        &mut min_ts,
                                        &mut max_ts,
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    Ok(ParsedSession {
        meta: SessionMeta {
            id: session_id,
            cwd: last_cwd,
            model: last_model,
            machine: whoami::fallible::hostname().unwrap_or_else(|_| "unknown".to_string()),
            turns,
            tokens_in,
            tokens_out,
            cost_usd,
            started: min_ts.unwrap_or_default(),
            updated: max_ts.unwrap_or_default(),
        },
        tool_calls,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn test_session_id_extraction() {
        assert_eq!(
            extract_session_id("rollout-2026-02-21T00-02-09-019c7d80-f629-7452-b38c-ac4ab228d44d.jsonl"),
            Some("019c7d80-f629-7452-b38c-ac4ab228d44d".to_string())
        );
        assert_eq!(
            extract_session_id("session-2026-04-28T08-24-df09c16b.jsonl"),
            Some("df09c16b".to_string())
        );
        assert_eq!(
            extract_session_id("chat_history.jsonl"),
            Some("chat_history".to_string())
        );
    }

    #[test]
    fn test_tool_classification() {
        let mut skills = HashSet::new();
        skills.insert("deep-research".to_string());

        assert_eq!(
            classify_tool("mcp__gmail__authenticate", false, &skills),
            ToolCat::Mcp("gmail".to_string())
        );
        assert_eq!(
            classify_tool("Task", false, &skills),
            ToolCat::Subagent
        );
        assert_eq!(
            classify_tool("Bash", true, &skills),
            ToolCat::Subagent
        );
        assert_eq!(
            classify_tool("deep-research", false, &skills),
            ToolCat::Skill
        );
        assert_eq!(
            classify_tool("Read", false, &skills),
            ToolCat::Builtin
        );
    }

    #[test]
    fn test_parse_mock_session() {
        let dir = tempfile::tempdir().unwrap();
        let session_file_path = dir.path().join("session-12345678.jsonl");

        let lines = vec![
            r#"{"type":"user","timestamp":"2026-06-30T00:00:00Z","cwd":"/workspace/test","message":{"role":"user","content":"hello"}}"#,
            r#"{"type":"attachment","attachment":{"type":"skill_listing","names":["custom-skill"]}}"#,
            r#"{"type":"assistant","timestamp":"2026-06-30T00:00:05Z","cwd":"/workspace/test","message":{"role":"assistant","model":"claude-sonnet-4-6","usage":{"input_tokens":100,"output_tokens":50},"content":[{"type":"tool_use","id":"1","name":"Bash","input":{"command":"ls"}},{"type":"tool_use","id":"2","name":"mcp__github__get_issue","input":{"id":78}},{"type":"tool_use","id":"3","name":"custom-skill","input":{}}]}}"#,
        ];

        let mut f = std::fs::File::create(&session_file_path).unwrap();
        for line in lines {
            writeln!(f, "{}", line).unwrap();
        }
        drop(f);

        let parsed = parse_session(&session_file_path).unwrap();
        assert_eq!(parsed.meta.id, "12345678");
        assert_eq!(parsed.meta.cwd, "/workspace/test");
        assert_eq!(parsed.meta.model, "claude-sonnet-4-6");
        assert_eq!(parsed.meta.turns, 1);
        assert_eq!(parsed.meta.tokens_in, 100);
        assert_eq!(parsed.meta.tokens_out, 50);
        assert!(parsed.meta.cost_usd > 0.0);
        assert_eq!(parsed.meta.started, "2026-06-30T00:00:00Z");
        assert_eq!(parsed.meta.updated, "2026-06-30T00:00:05Z");

        assert_eq!(parsed.tool_calls.len(), 3);
        assert_eq!(parsed.tool_calls[0].tool, "Bash");
        assert_eq!(parsed.tool_calls[0].category, ToolCat::Builtin);
        assert_eq!(parsed.tool_calls[1].tool, "mcp__github__get_issue");
        assert_eq!(parsed.tool_calls[1].category, ToolCat::Mcp("github".to_string()));
        assert_eq!(parsed.tool_calls[2].tool, "custom-skill");
        assert_eq!(parsed.tool_calls[2].category, ToolCat::Skill);
    }
}
