use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Sequence counter for message ordering
pub type SeqNum = u32;

/// All message types exchanged via the comms directory
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Message {
    /// Broker -> Worker: new conversation turn with tool calls to execute
    Turn {
        seq: SeqNum,
        id: String,
        tool_calls: Vec<ToolCall>,
    },
    /// Worker -> Broker: result of executing a tool
    ToolRequest {
        seq: SeqNum,
        id: String,
        tool_call_id: String,
        tool_name: String,
        result: ToolOutput,
    },
    /// Broker -> Worker: tool result acknowledgement (for multi-step)
    ToolResult {
        seq: SeqNum,
        id: String,
        tool_call_id: String,
        content: String,
    },
    /// Worker -> Broker: all tool calls for this turn are done
    TurnComplete {
        seq: SeqNum,
        id: String,
        results: Vec<ToolOutput>,
    },
    /// Either direction: error
    Error {
        seq: SeqNum,
        id: String,
        message: String,
    },
    /// Worker -> Broker: heartbeat
    Heartbeat {
        seq: SeqNum,
        timestamp: String,
    },
    /// Broker -> Worker: graceful shutdown
    Shutdown {
        seq: SeqNum,
        id: String,
    },
}

/// A tool call from the LLM
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub input: serde_json::Value,
}

/// Output from executing a tool
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolOutput {
    pub tool_call_id: String,
    pub success: bool,
    pub output: String,
    #[serde(default)]
    pub error: Option<String>,
}

impl Message {
    /// Get the sequence number
    pub fn seq(&self) -> SeqNum {
        match self {
            Message::Turn { seq, .. }
            | Message::ToolRequest { seq, .. }
            | Message::ToolResult { seq, .. }
            | Message::TurnComplete { seq, .. }
            | Message::Error { seq, .. }
            | Message::Heartbeat { seq, .. }
            | Message::Shutdown { seq, .. } => *seq,
        }
    }

    /// Filename for this message: {seq:03}-{type}-{id}.json
    pub fn filename(&self) -> String {
        let seq = self.seq();
        let (kind, id) = match self {
            Message::Turn { id, .. } => ("turn", id.as_str()),
            Message::ToolRequest { id, .. } => ("tool_request", id.as_str()),
            Message::ToolResult { id, .. } => ("tool_result", id.as_str()),
            Message::TurnComplete { id, .. } => ("turn_complete", id.as_str()),
            Message::Error { id, .. } => ("error", id.as_str()),
            Message::Heartbeat { timestamp, .. } => ("heartbeat", timestamp.as_str()),
            Message::Shutdown { id, .. } => ("shutdown", id.as_str()),
        };
        // Truncate id for filename safety
        let short_id = if id.len() > 8 { &id[..8] } else { id };
        format!("{seq:03}-{kind}-{short_id}.json")
    }
}

/// Write a message atomically to a directory (write .tmp then rename)
pub fn send_message(dir: &Path, msg: &Message) -> Result<PathBuf> {
    let filename = msg.filename();
    let target = dir.join(&filename);
    let tmp = dir.join(format!(".{filename}.tmp"));

    let json = serde_json::to_string_pretty(msg).context("serializing message")?;
    std::fs::write(&tmp, json).context("writing temp message file")?;
    std::fs::rename(&tmp, &target).context("renaming message file")?;

    Ok(target)
}

/// Read all messages from a directory, sorted by sequence number
pub fn read_messages(dir: &Path) -> Result<Vec<Message>> {
    let mut messages = Vec::new();

    if !dir.is_dir() {
        return Ok(messages);
    }

    for entry in std::fs::read_dir(dir).context("reading message directory")? {
        let entry = entry?;
        let path = entry.path();

        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }

        // Skip temp files
        if path
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.starts_with('.'))
            .unwrap_or(false)
        {
            continue;
        }

        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;

        match serde_json::from_str::<Message>(&content) {
            Ok(msg) => messages.push(msg),
            Err(e) => {
                tracing::warn!(file = %path.display(), error = %e, "skipping malformed message");
            }
        }
    }

    messages.sort_by_key(|m| m.seq());
    Ok(messages)
}

/// Read and consume (delete) all messages from a directory
pub fn consume_messages(dir: &Path) -> Result<Vec<Message>> {
    let messages = read_messages(dir)?;

    // Delete consumed files
    if dir.is_dir() {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("json") {
                let _ = std::fs::remove_file(&path);
            }
        }
    }

    Ok(messages)
}

/// Watch a directory for new .json files. Returns when a new message appears.
/// Uses polling (simple, works everywhere including inside containers).
pub async fn wait_for_message(dir: &Path, poll_ms: u64) -> Result<Message> {
    use std::collections::HashSet;

    let mut seen: HashSet<String> = HashSet::new();

    // Record existing files
    if dir.is_dir() {
        for entry in std::fs::read_dir(dir)? {
            if let Ok(entry) = entry {
                if let Some(name) = entry.file_name().to_str() {
                    seen.insert(name.to_string());
                }
            }
        }
    }

    loop {
        tokio::time::sleep(std::time::Duration::from_millis(poll_ms)).await;

        if !dir.is_dir() {
            continue;
        }

        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().to_string();

            if name.starts_with('.') || !name.ends_with(".json") {
                continue;
            }

            if seen.contains(&name) {
                continue;
            }

            seen.insert(name);

            let path = entry.path();
            let content = std::fs::read_to_string(&path)?;
            match serde_json::from_str::<Message>(&content) {
                Ok(msg) => return Ok(msg),
                Err(e) => {
                    tracing::warn!(file = %path.display(), error = %e, "malformed message");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_turn(seq: SeqNum) -> Message {
        Message::Turn {
            seq,
            id: "test-id".to_string(),
            tool_calls: vec![ToolCall {
                id: "tc-1".to_string(),
                name: "bash".to_string(),
                input: serde_json::json!({"command": "echo hello"}),
            }],
        }
    }

    #[test]
    fn test_message_roundtrip() {
        let msg = make_turn(1);
        let json = serde_json::to_string(&msg).unwrap();
        let parsed: Message = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.seq(), 1);
    }

    #[test]
    fn test_filename_format() {
        let msg = make_turn(1);
        let filename = msg.filename();
        assert!(filename.starts_with("001-turn-"));
        assert!(filename.ends_with(".json"));
    }

    #[test]
    fn test_send_and_read() {
        let dir = tempfile::tempdir().unwrap();
        let msg = make_turn(1);

        send_message(dir.path(), &msg).unwrap();

        let messages = read_messages(dir.path()).unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].seq(), 1);
    }

    #[test]
    fn test_consume_messages() {
        let dir = tempfile::tempdir().unwrap();
        send_message(dir.path(), &make_turn(1)).unwrap();
        send_message(dir.path(), &make_turn(2)).unwrap();

        let messages = consume_messages(dir.path()).unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].seq(), 1);
        assert_eq!(messages[1].seq(), 2);

        // Should be empty now
        let remaining = read_messages(dir.path()).unwrap();
        assert!(remaining.is_empty());
    }

    #[test]
    fn test_read_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let messages = read_messages(dir.path()).unwrap();
        assert!(messages.is_empty());
    }

    #[test]
    fn test_message_types_serialize() {
        let messages = vec![
            Message::Turn {
                seq: 1,
                id: "a".to_string(),
                tool_calls: vec![],
            },
            Message::ToolRequest {
                seq: 2,
                id: "b".to_string(),
                tool_call_id: "tc1".to_string(),
                tool_name: "bash".to_string(),
                result: ToolOutput {
                    tool_call_id: "tc1".to_string(),
                    success: true,
                    output: "hello".to_string(),
                    error: None,
                },
            },
            Message::TurnComplete {
                seq: 3,
                id: "c".to_string(),
                results: vec![],
            },
            Message::Error {
                seq: 4,
                id: "d".to_string(),
                message: "oops".to_string(),
            },
            Message::Heartbeat {
                seq: 5,
                timestamp: "2026-01-01T00:00:00Z".to_string(),
            },
            Message::Shutdown {
                seq: 6,
                id: "e".to_string(),
            },
        ];

        for msg in &messages {
            let json = serde_json::to_string(msg).unwrap();
            let parsed: Message = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed.seq(), msg.seq());
        }
    }
}
