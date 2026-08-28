//! Minimal stdio MCP (JSON-RPC 2.0) client — enough for n8 to spawn a capsule
//! container and drive it from the host: `initialize`, `tools/list`,
//! `tools/call`. Newline-delimited JSON per the MCP stdio transport.
//!
//! This is the host-side driver behind `n8 capsule run`: n8 IS the MCP client,
//! the capsule is the stdio server, connected by the child process's pipe — no
//! docker socket, no network. See docs/plans/iterative-baking-nebula.

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::{Duration, Instant};

pub struct McpClient {
    child: Child,
    stdin: Option<std::process::ChildStdin>,
    rx: Receiver<String>,
    next_id: i64,
    timeout: Duration,
}

#[derive(Debug, Clone)]
pub struct ToolInfo {
    pub name: String,
    pub description: String,
}

impl McpClient {
    /// Spawn `runtime args…` (e.g. `docker run --rm -i … <image>`) with piped
    /// stdio and a background stdout line-reader. stderr is inherited so the
    /// capsule's own banner/logs are visible.
    pub fn spawn(runtime: &str, args: &[String]) -> Result<Self> {
        let mut child = Command::new(runtime)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| format!("spawning `{runtime}` (is the runtime installed?)"))?;
        let stdin = child.stdin.take().context("child has no stdin")?;
        let stdout = child.stdout.take().context("child has no stdout")?;
        let (tx, rx) = channel::<String>();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                match line {
                    Ok(l) => {
                        if tx.send(l).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        Ok(Self {
            child,
            stdin: Some(stdin),
            rx,
            next_id: 0,
            timeout: Duration::from_secs(60),
        })
    }

    pub fn with_timeout(mut self, secs: u64) -> Self {
        self.timeout = Duration::from_secs(secs.max(1));
        self
    }

    fn send(&mut self, msg: &Value) -> Result<()> {
        let stdin = self.stdin.as_mut().context("capsule stdin already closed")?;
        let line = serde_json::to_string(msg)?;
        stdin.write_all(line.as_bytes())?;
        stdin.write_all(b"\n")?;
        stdin.flush()?;
        Ok(())
    }

    /// Send a request and wait for the response with the matching id, skipping
    /// notifications / other-id frames / non-JSON log lines until the timeout.
    fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&request_msg(id, method, params))?;
        let deadline = Instant::now() + self.timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                bail!("timed out waiting for `{method}` response");
            }
            match self.rx.recv_timeout(remaining) {
                Ok(line) => match match_response(&line, id) {
                    Some(res) => return res,
                    None => continue, // notification / other id / log noise
                },
                Err(RecvTimeoutError::Timeout) => {
                    bail!("timed out waiting for `{method}` response")
                }
                Err(RecvTimeoutError::Disconnected) => {
                    bail!("capsule exited before answering `{method}`")
                }
            }
        }
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        self.send(&json!({"jsonrpc": "2.0", "method": method, "params": params}))
    }

    /// MCP initialize handshake (request + the `initialized` notification).
    pub fn initialize(&mut self) -> Result<Value> {
        let result = self.request(
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "n8-capsule-run", "version": env!("CARGO_PKG_VERSION")}
            }),
        )?;
        self.notify("notifications/initialized", json!({}))?;
        Ok(result)
    }

    pub fn list_tools(&mut self) -> Result<Vec<ToolInfo>> {
        let result = self.request("tools/list", json!({}))?;
        Ok(parse_tools(&result))
    }

    pub fn call_tool(&mut self, name: &str, arguments: Value) -> Result<Value> {
        self.request("tools/call", json!({"name": name, "arguments": arguments}))
    }
}

impl Drop for McpClient {
    /// Close stdin (EOF → the server exits) and reap the child on every path.
    fn drop(&mut self) {
        self.stdin.take();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn request_msg(id: i64, method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
}

/// Classify one incoming line against the awaited id: `Some(Ok/Err)` when it's
/// the matching response, `None` when it's noise to skip.
fn match_response(line: &str, id: i64) -> Option<Result<Value>> {
    let val: Value = serde_json::from_str(line).ok()?;
    if val.get("id").and_then(Value::as_i64) != Some(id) {
        return None;
    }
    if let Some(err) = val.get("error") {
        return Some(Err(anyhow::anyhow!("{err}")));
    }
    Some(Ok(val.get("result").cloned().unwrap_or(Value::Null)))
}

fn parse_tools(result: &Value) -> Vec<ToolInfo> {
    result
        .get("tools")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .map(|t| ToolInfo {
                    name: t.get("name").and_then(Value::as_str).unwrap_or("").to_string(),
                    description: t
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Pull human-readable text out of a `tools/call` result's `content` array.
pub fn tool_result_text(result: &Value) -> String {
    result
        .get("content")
        .and_then(Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| serde_json::to_string_pretty(result).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_msg_shape() {
        let m = request_msg(7, "tools/list", json!({}));
        assert_eq!(m["jsonrpc"], "2.0");
        assert_eq!(m["id"], 7);
        assert_eq!(m["method"], "tools/list");
    }

    #[test]
    fn match_response_matches_id_and_extracts_result() {
        let line = r#"{"jsonrpc":"2.0","id":3,"result":{"ok":true}}"#;
        assert!(match_response(line, 2).is_none()); // wrong id → skip
        let got = match_response(line, 3).unwrap().unwrap();
        assert_eq!(got["ok"], true);
    }

    #[test]
    fn match_response_skips_notifications_and_noise() {
        assert!(match_response(r#"{"jsonrpc":"2.0","method":"notifications/x"}"#, 1).is_none());
        assert!(match_response("serving on stdio", 1).is_none()); // non-JSON log
    }

    #[test]
    fn match_response_surfaces_errors() {
        let line = r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"nope"}}"#;
        assert!(match_response(line, 1).unwrap().is_err());
    }

    #[test]
    fn parse_tools_and_text() {
        let r = json!({"tools":[{"name":"sigil_ask","description":"ask"},{"name":"sigil_build"}]});
        let tools = parse_tools(&r);
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].name, "sigil_ask");
        assert_eq!(tools[1].description, "");
        let txt = tool_result_text(&json!({"content":[{"type":"text","text":"hello"}]}));
        assert_eq!(txt, "hello");
    }
}
