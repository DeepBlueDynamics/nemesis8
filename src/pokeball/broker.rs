use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use super::protocol::{self, Message, SeqNum, ToolCall};
use super::spec::PokeballSpec;

/// A message in the conversation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationMessage {
    pub role: String,
    pub content: serde_json::Value,
}

/// Response from the LLM
#[derive(Debug)]
pub enum LlmResponse {
    /// LLM wants to call tools
    ToolUse {
        tool_calls: Vec<ToolCall>,
    },
    /// LLM is done (final text response)
    Done {
        text: String,
    },
}

/// Anthropic Claude API provider
pub struct AnthropicProvider {
    api_key: String,
    model: String,
    tools: Vec<serde_json::Value>,
    system_prompt: String,
}

impl AnthropicProvider {
    pub fn new(spec: &PokeballSpec) -> Result<Self> {
        let api_key = std::env::var(&spec.provider.api_key_env).with_context(|| {
            format!(
                "missing API key: set {} environment variable",
                spec.provider.api_key_env
            )
        })?;

        let tools = build_tool_definitions(&spec.tools.allow);
        let system_prompt = format!(
            "You are working on the '{}' project. {}. \
             Your workspace is at /work. Use the provided tools to accomplish tasks. \
             Be concise and focused.",
            spec.metadata.name, spec.metadata.description,
        );

        Ok(Self {
            api_key,
            model: spec.provider.model.clone(),
            tools,
            system_prompt,
        })
    }

    pub fn with_api_key(mut self, key: String) -> Self {
        self.api_key = key;
        self
    }
}

// We can't use async_trait without adding the dep, so we'll use a manual impl
impl AnthropicProvider {
    pub async fn send_turn(&self, messages: &[ConversationMessage]) -> Result<LlmResponse> {
        let client = reqwest::Client::new();

        let body = serde_json::json!({
            "model": self.model,
            "max_tokens": 8192,
            "system": self.system_prompt,
            "tools": self.tools,
            "messages": messages,
        });

        let response = client
            .post("https://api.anthropic.com/v1/messages")
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .context("sending request to Claude API")?;

        let status = response.status();
        if !status.is_success() {
            let error_body = response.text().await.unwrap_or_default();
            anyhow::bail!("Claude API error ({}): {}", status, error_body);
        }

        let resp: serde_json::Value = response.json().await.context("parsing Claude response")?;

        parse_anthropic_response(&resp)
    }
}

/// Parse the Anthropic API response into our LlmResponse type
fn parse_anthropic_response(resp: &serde_json::Value) -> Result<LlmResponse> {
    let stop_reason = resp
        .get("stop_reason")
        .and_then(|v| v.as_str())
        .unwrap_or("end_turn");

    let content = resp
        .get("content")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow::anyhow!("missing content array in response"))?;

    if stop_reason == "tool_use" {
        let mut tool_calls = Vec::new();
        for block in content {
            if block.get("type").and_then(|v| v.as_str()) == Some("tool_use") {
                let id = block
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let name = block
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let input = block.get("input").cloned().unwrap_or(serde_json::json!({}));
                tool_calls.push(ToolCall { id, name, input });
            }
        }
        Ok(LlmResponse::ToolUse { tool_calls })
    } else {
        // Extract text
        let mut text = String::new();
        for block in content {
            if block.get("type").and_then(|v| v.as_str()) == Some("text") {
                if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
                    text.push_str(t);
                }
            }
        }
        Ok(LlmResponse::Done { text })
    }
}

/// Build Claude tool definitions from the spec's allowed tools
fn build_tool_definitions(
    tools: &[super::spec::ToolEntry],
) -> Vec<serde_json::Value> {
    tools
        .iter()
        .filter_map(|t| tool_schema(&t.name))
        .collect()
}

fn tool_schema(name: &str) -> Option<serde_json::Value> {
    match name {
        "bash" => Some(serde_json::json!({
            "name": "bash",
            "description": "Execute a bash command in the project directory",
            "input_schema": {
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "The bash command to execute"
                    },
                    "timeout": {
                        "type": "integer",
                        "description": "Timeout in milliseconds (default 120000)"
                    }
                },
                "required": ["command"]
            }
        })),
        "file_read" => Some(serde_json::json!({
            "name": "file_read",
            "description": "Read a file from the project",
            "input_schema": {
                "type": "object",
                "properties": {
                    "file_path": {
                        "type": "string",
                        "description": "Path to the file to read"
                    },
                    "offset": {
                        "type": "integer",
                        "description": "Line offset to start reading from"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Max lines to read"
                    }
                },
                "required": ["file_path"]
            }
        })),
        "file_write" => Some(serde_json::json!({
            "name": "file_write",
            "description": "Write content to a file in the project",
            "input_schema": {
                "type": "object",
                "properties": {
                    "file_path": {
                        "type": "string",
                        "description": "Path to the file to write"
                    },
                    "content": {
                        "type": "string",
                        "description": "Content to write"
                    }
                },
                "required": ["file_path", "content"]
            }
        })),
        "grep" => Some(serde_json::json!({
            "name": "grep",
            "description": "Search file contents for a pattern",
            "input_schema": {
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "Regex pattern to search for"
                    },
                    "path": {
                        "type": "string",
                        "description": "Directory or file to search in"
                    },
                    "glob": {
                        "type": "string",
                        "description": "File glob filter (e.g. '*.ts')"
                    }
                },
                "required": ["pattern"]
            }
        })),
        "glob" => Some(serde_json::json!({
            "name": "glob",
            "description": "Find files matching a glob pattern",
            "input_schema": {
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "Glob pattern (e.g. '*.ts', '**/*.json')"
                    },
                    "path": {
                        "type": "string",
                        "description": "Directory to search in"
                    }
                },
                "required": ["pattern"]
            }
        })),
        _ => None,
    }
}

/// The broker: coordinates between the LLM and the worker container
pub struct Broker {
    provider: AnthropicProvider,
    comms_dir: PathBuf,
    seq: SeqNum,
    conversation: Vec<ConversationMessage>,
    timeout_minutes: u64,
}

impl Broker {
    pub fn new(
        provider: AnthropicProvider,
        comms_dir: PathBuf,
        timeout_minutes: u64,
    ) -> Self {
        Self {
            provider,
            comms_dir,
            seq: 0,
            conversation: Vec::new(),
            timeout_minutes,
        }
    }

    fn next_seq(&mut self) -> SeqNum {
        self.seq += 1;
        self.seq
    }

    fn inbox(&self) -> PathBuf {
        self.comms_dir.join("inbox")
    }

    fn outbox(&self) -> PathBuf {
        self.comms_dir.join("outbox")
    }

    /// Run a single prompt through the broker-worker loop
    pub async fn run(&mut self, prompt: &str) -> Result<String> {
        // Add user message
        self.conversation.push(ConversationMessage {
            role: "user".to_string(),
            content: serde_json::json!(prompt),
        });

        let deadline = tokio::time::Instant::now()
            + std::time::Duration::from_secs(self.timeout_minutes * 60);

        loop {
            if tokio::time::Instant::now() > deadline {
                // Send shutdown
                let seq = self.next_seq();
                let shutdown = Message::Shutdown {
                    seq,
                    id: uuid::Uuid::new_v4().to_string(),
                };
                protocol::send_message(&self.inbox(), &shutdown)?;
                anyhow::bail!("pokeball timed out after {} minutes", self.timeout_minutes);
            }

            // Send to LLM
            let response = self.provider.send_turn(&self.conversation).await?;

            match response {
                LlmResponse::Done { text } => {
                    // Send shutdown to worker
                    let seq = self.next_seq();
                    let shutdown = Message::Shutdown {
                        seq,
                        id: uuid::Uuid::new_v4().to_string(),
                    };
                    protocol::send_message(&self.inbox(), &shutdown)?;

                    return Ok(text);
                }
                LlmResponse::ToolUse { tool_calls } => {
                    // Send tool calls to worker via inbox
                    let seq = self.next_seq();
                    let turn = Message::Turn {
                        seq,
                        id: uuid::Uuid::new_v4().to_string(),
                        tool_calls: tool_calls.clone(),
                    };
                    protocol::send_message(&self.inbox(), &turn)?;

                    // Wait for worker response
                    let response_msg =
                        protocol::wait_for_message(&self.outbox(), 100).await?;

                    // Process worker response
                    match response_msg {
                        Message::TurnComplete { results, .. } => {
                            // Add assistant message with tool use
                            let mut content_blocks = Vec::new();
                            for tc in &tool_calls {
                                content_blocks.push(serde_json::json!({
                                    "type": "tool_use",
                                    "id": tc.id,
                                    "name": tc.name,
                                    "input": tc.input,
                                }));
                            }
                            self.conversation.push(ConversationMessage {
                                role: "assistant".to_string(),
                                content: serde_json::Value::Array(content_blocks),
                            });

                            // Add tool results
                            let mut result_blocks = Vec::new();
                            for r in &results {
                                let content = if r.success {
                                    &r.output
                                } else {
                                    r.error.as_deref().unwrap_or("unknown error")
                                };
                                result_blocks.push(serde_json::json!({
                                    "type": "tool_result",
                                    "tool_use_id": r.tool_call_id,
                                    "content": content,
                                }));
                            }
                            self.conversation.push(ConversationMessage {
                                role: "user".to_string(),
                                content: serde_json::Value::Array(result_blocks),
                            });
                        }
                        Message::Error { message, .. } => {
                            anyhow::bail!("worker error: {message}");
                        }
                        other => {
                            tracing::warn!("unexpected message from worker: {:?}", other);
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tool_schema_bash() {
        let schema = tool_schema("bash").unwrap();
        assert_eq!(schema["name"], "bash");
        assert!(schema["input_schema"]["properties"]["command"].is_object());
    }

    #[test]
    fn test_tool_schema_unknown() {
        assert!(tool_schema("nonexistent").is_none());
    }

    #[test]
    fn test_parse_anthropic_response_text() {
        let resp = serde_json::json!({
            "stop_reason": "end_turn",
            "content": [
                {"type": "text", "text": "Hello world"}
            ]
        });
        match parse_anthropic_response(&resp).unwrap() {
            LlmResponse::Done { text } => assert_eq!(text, "Hello world"),
            _ => panic!("expected Done"),
        }
    }

    #[test]
    fn test_parse_anthropic_response_tool_use() {
        let resp = serde_json::json!({
            "stop_reason": "tool_use",
            "content": [
                {
                    "type": "tool_use",
                    "id": "tc_123",
                    "name": "bash",
                    "input": {"command": "ls"}
                }
            ]
        });
        match parse_anthropic_response(&resp).unwrap() {
            LlmResponse::ToolUse { tool_calls } => {
                assert_eq!(tool_calls.len(), 1);
                assert_eq!(tool_calls[0].name, "bash");
                assert_eq!(tool_calls[0].id, "tc_123");
            }
            _ => panic!("expected ToolUse"),
        }
    }

    #[test]
    fn test_build_tool_definitions() {
        let tools = vec![
            super::super::spec::ToolEntry { name: "bash".to_string() },
            super::super::spec::ToolEntry { name: "file_read".to_string() },
            super::super::spec::ToolEntry { name: "unknown".to_string() },
        ];
        let defs = build_tool_definitions(&tools);
        assert_eq!(defs.len(), 2); // unknown filtered out
    }
}
