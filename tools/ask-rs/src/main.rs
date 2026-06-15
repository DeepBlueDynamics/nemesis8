//! ask — a one-shot "second opinion" MCP server for nemesis8 agents.
//!
//! Replaces ask.py with a single self-contained Rust binary that speaks the MCP
//! stdio transport (newline-delimited JSON-RPC 2.0) directly — same hand-rolled
//! pattern as nuts-files/shivvr, no SDK, no async runtime. One tool, `ask`,
//! dispatches a prompt to Claude / Gemini / OpenAI and returns the reply.
//!
//! API keys are read from the container env (forwarded by n8's build_env):
//!   ANTHROPIC_API_KEY · OPENAI_API_KEY · GEMINI_API_KEY (or GOOGLE_API_KEY).

use serde_json::{json, Value};
use std::io::{self, BufRead, Write};
use std::time::Duration;

const SERVER_NAME: &str = "ask";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");
const PROTOCOL_VERSION: &str = "2025-06-18";

fn main() {
    let stdin = io::stdin();
    let mut out = io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let req: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let id = req.get("id").cloned();
        let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");

        let response = match method {
            "initialize" => {
                // Echo the client's protocolVersion — strict clients (agy) list
                // tools but refuse tools/call if we advertise a version they
                // didn't offer. (Same lesson as nuts-files/shivvr.)
                let client_pv = req
                    .get("params")
                    .and_then(|p| p.get("protocolVersion"))
                    .and_then(|v| v.as_str())
                    .unwrap_or(PROTOCOL_VERSION);
                Some(ok(
                    id,
                    json!({
                        "protocolVersion": client_pv,
                        "capabilities": { "tools": {} },
                        "serverInfo": { "name": SERVER_NAME, "version": SERVER_VERSION },
                    }),
                ))
            }
            "tools/list" => Some(ok(id, json!({ "tools": tool_list() }))),
            "tools/call" => Some(handle_call(id, req.get("params"))),
            "ping" => Some(ok(id, json!({}))),
            _ if id.is_some() => Some(err(id, -32601, &format!("method not found: {method}"))),
            _ => None,
        };

        if let Some(resp) = response {
            let _ = writeln!(out, "{resp}");
            let _ = out.flush();
        }
    }
}

fn ok(id: Option<Value>, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}
fn err(id: Option<Value>, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn handle_call(id: Option<Value>, params: Option<&Value>) -> Value {
    let name = params
        .and_then(|p| p.get("name"))
        .and_then(|n| n.as_str())
        .unwrap_or("");
    let args = params
        .and_then(|p| p.get("arguments"))
        .cloned()
        .unwrap_or(json!({}));
    let res = match name {
        "ask" => ask(&args),
        _ => Err(format!("unknown tool: {name}")),
    };
    match res {
        Ok(text) => ok(id, json!({ "content": [{ "type": "text", "text": text }] })),
        Err(e) => ok(
            id,
            json!({ "content": [{ "type": "text", "text": format!("error: {e}") }], "isError": true }),
        ),
    }
}

fn tool_list() -> Vec<Value> {
    let p_str = |d: &str| json!({ "type": "string", "description": d });
    let p_int = |d: &str| json!({ "type": "integer", "description": d });
    let p_num = |d: &str| json!({ "type": "number", "description": d });
    vec![json!({
        "name": "ask",
        "description": "Get a one-shot SECOND OPINION from a different model — Claude, Gemini, or \
            OpenAI. Each call is independent (no conversation history). Use it to sanity-check an \
            approach, get a fresh take, or cross-examine your own answer with another model. \
            Provider auto-detects from the model name, or pass `provider` explicitly. Keys are \
            read from the container env (ANTHROPIC_API_KEY / OPENAI_API_KEY / GEMINI_API_KEY).",
        "inputSchema": {
            "type": "object",
            "properties": {
                "prompt": p_str("the question / prompt to send"),
                "provider": p_str("claude|anthropic, gemini|google, gpt|openai. Omitted → inferred from model, else required"),
                "model": p_str("explicit model id (e.g. claude-3-5-sonnet-latest, gpt-4o, gemini-2.5-pro). Omitted → provider default"),
                "system": p_str("optional system prompt / instruction"),
                "max_tokens": p_int("response length cap (default 2048)"),
                "temperature": p_num("optional sampling temperature"),
            },
            "required": ["prompt"]
        }
    })]
}

// ── the ask tool ──────────────────────────────────────────────────────────────

fn ask(a: &Value) -> Result<String, String> {
    let prompt = sreq(a, "prompt")?;
    let system = a.get("system").and_then(|v| v.as_str());
    let max_tokens = a.get("max_tokens").and_then(|v| v.as_u64());
    let temperature = a.get("temperature").and_then(|v| v.as_f64());
    let model_arg = a.get("model").and_then(|v| v.as_str()).map(String::from);

    // Resolve provider: explicit arg (canonicalized), else inferred from model.
    let provider = a
        .get("provider")
        .and_then(|v| v.as_str())
        .and_then(canonicalize_provider)
        .or_else(|| model_arg.as_deref().and_then(auto_detect_provider))
        .ok_or_else(|| {
            "could not resolve provider — pass provider (claude|gemini|gpt) or a recognizable model"
                .to_string()
        })?;

    let model = model_arg.unwrap_or_else(|| default_model(&provider).to_string());

    let (text, in_tok, out_tok) = match provider.as_str() {
        "gpt" => query_openai(&prompt, &model, system, max_tokens, temperature),
        "claude" => query_anthropic(&prompt, &model, system, max_tokens, temperature),
        "gemini" => query_gemini(&prompt, &model, system, max_tokens, temperature),
        _ => Err("unsupported provider".to_string()),
    }?;

    let usage = match (in_tok, out_tok) {
        (Some(i), Some(o)) => format!(" · {i}+{o} tok"),
        _ => String::new(),
    };
    Ok(format!("[{provider}/{model}{usage}]\n\n{}", text.trim()))
}

fn auto_detect_provider(model: &str) -> Option<String> {
    let m = model.to_lowercase();
    if m.contains("gpt") || m.contains("o1-") || m.contains("o3-") {
        Some("gpt".into())
    } else if m.contains("claude") {
        Some("claude".into())
    } else if m.contains("gemini") {
        Some("gemini".into())
    } else {
        None
    }
}

fn canonicalize_provider(p: &str) -> Option<String> {
    match p.trim().to_lowercase().as_str() {
        "claude" | "anthropic" => Some("claude".into()),
        "gemini" | "google" => Some("gemini".into()),
        "gpt" | "openai" | "chatgpt" => Some("gpt".into()),
        _ => None,
    }
}

fn default_model(provider: &str) -> &'static str {
    match provider {
        "claude" => "claude-3-5-sonnet-latest",
        "gemini" => "gemini-2.5-pro",
        _ => "gpt-4o",
    }
}

// ── provider HTTP calls (blocking ureq + rustls) ───────────────────────────────

type AskResult = Result<(String, Option<i64>, Option<i64>), String>;

fn query_openai(
    prompt: &str,
    model: &str,
    system: Option<&str>,
    max_tokens: Option<u64>,
    temperature: Option<f64>,
) -> AskResult {
    let key = env_key(&["OPENAI_API_KEY"])?;
    let mut messages = Vec::new();
    if let Some(sys) = system {
        messages.push(json!({ "role": "system", "content": sys }));
    }
    messages.push(json!({ "role": "user", "content": prompt }));
    let mut body = json!({ "model": model, "messages": messages });
    if let Some(t) = max_tokens {
        body["max_tokens"] = json!(t);
    }
    if let Some(t) = temperature {
        body["temperature"] = json!(t);
    }
    let v = post_json(
        "https://api.openai.com/v1/chat/completions",
        &[("Authorization", &format!("Bearer {key}"))],
        &body,
    )?;
    let text = v["choices"][0]["message"]["content"].as_str().unwrap_or("").to_string();
    Ok((
        text,
        v["usage"]["prompt_tokens"].as_i64(),
        v["usage"]["completion_tokens"].as_i64(),
    ))
}

fn query_anthropic(
    prompt: &str,
    model: &str,
    system: Option<&str>,
    max_tokens: Option<u64>,
    temperature: Option<f64>,
) -> AskResult {
    let key = env_key(&["ANTHROPIC_API_KEY"])?;
    let mut body = json!({
        "model": model,
        "messages": [{ "role": "user", "content": prompt }],
        "max_tokens": max_tokens.unwrap_or(2048),
    });
    if let Some(sys) = system {
        body["system"] = json!(sys);
    }
    if let Some(t) = temperature {
        body["temperature"] = json!(t);
    }
    let v = post_json(
        "https://api.anthropic.com/v1/messages",
        &[("x-api-key", &key), ("anthropic-version", "2023-06-01")],
        &body,
    )?;
    let mut text = String::new();
    if let Some(arr) = v["content"].as_array() {
        for item in arr {
            if item["type"].as_str() == Some("text") {
                if let Some(t) = item["text"].as_str() {
                    text.push_str(t);
                }
            }
        }
    }
    Ok((
        text,
        v["usage"]["input_tokens"].as_i64(),
        v["usage"]["output_tokens"].as_i64(),
    ))
}

fn query_gemini(
    prompt: &str,
    model: &str,
    system: Option<&str>,
    max_tokens: Option<u64>,
    temperature: Option<f64>,
) -> AskResult {
    let key = env_key(&["GEMINI_API_KEY", "GOOGLE_API_KEY"])?;
    let mut body = json!({ "contents": [{ "parts": [{ "text": prompt }] }] });
    if let Some(sys) = system {
        body["systemInstruction"] = json!({ "parts": [{ "text": sys }] });
    }
    let mut cfg = json!({});
    if let Some(t) = max_tokens {
        cfg["maxOutputTokens"] = json!(t);
    }
    if let Some(t) = temperature {
        cfg["temperature"] = json!(t);
    }
    if cfg.as_object().map_or(false, |o| !o.is_empty()) {
        body["generationConfig"] = cfg;
    }
    // Key goes in the x-goog-api-key header rather than the URL (keeps it out of logs).
    let url = format!("https://generativelanguage.googleapis.com/v1beta/models/{model}:generateContent");
    let v = post_json(&url, &[("x-goog-api-key", &key)], &body)?;
    let text = v["candidates"][0]["content"]["parts"][0]["text"].as_str().unwrap_or("").to_string();
    Ok((
        text,
        v["usageMetadata"]["promptTokenCount"].as_i64(),
        v["usageMetadata"]["candidatesTokenCount"].as_i64(),
    ))
}

// ── helpers ────────────────────────────────────────────────────────────────

/// POST JSON, return the parsed response. Surfaces API error bodies on non-2xx.
fn post_json(url: &str, headers: &[(&str, &str)], body: &Value) -> Result<Value, String> {
    let mut req = ureq::post(url)
        .set("Content-Type", "application/json")
        .timeout(Duration::from_secs(120));
    for (k, v) in headers {
        req = req.set(k, v);
    }
    match req.send_string(&body.to_string()) {
        Ok(resp) => {
            let txt = resp.into_string().map_err(|e| format!("reading response: {e}"))?;
            serde_json::from_str(&txt).map_err(|e| format!("non-JSON response: {e}"))
        }
        Err(ureq::Error::Status(code, resp)) => {
            let txt = resp.into_string().unwrap_or_default();
            Err(format!("HTTP {code}: {}", truncate(&txt, 400)))
        }
        Err(e) => Err(format!("request failed: {e}")),
    }
}

/// First of `names` set in the env, or a clear "not set" error listing them.
fn env_key(names: &[&str]) -> Result<String, String> {
    for n in names {
        if let Ok(v) = std::env::var(n) {
            if !v.trim().is_empty() {
                return Ok(v);
            }
        }
    }
    Err(format!("{} not set in the container env", names.join(" / ")))
}

fn sreq(a: &Value, k: &str) -> Result<String, String> {
    a.get(k)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("missing required string arg: {k}"))
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n).collect::<String>())
    }
}
