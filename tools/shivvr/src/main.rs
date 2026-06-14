//! shivvr — an MCP server that gives nemesis8 agents local access to the shivvr
//! embeddings service.
//!
//! shivvr is the sovereign embeddings engine (the same one delos-broker uses for
//! memory ingestion). This tool is a thin, self-contained Rust shim the agent
//! runs in-container: it speaks the MCP stdio transport (newline-delimited
//! JSON-RPC 2.0) directly and forwards to shivvr's local HTTP API
//! (`POST {SHIVVR_URL}/memory/_mcp/ingest`), so the agent gets clean MCP tools
//! instead of hand-rolling HTTP.
//!
//! Tools:
//!   - shivvr_embed       text → embedding (summary; full vector on request)
//!   - shivvr_similarity  two texts → cosine similarity (the agent-friendly op)
//!   - shivvr_status      where it points + reachability + embedding dimension
//!
//! Target: set SHIVVR_URL to the local shivvr (e.g. http://host.docker.internal:8000
//! for a host-run instance, or http://shivvr:<port> on the gnosis-network).

use serde_json::{json, Value};
use std::io::{self, BufRead, Write};
use std::time::Duration;

const SERVER_NAME: &str = "shivvr";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");
const PROTOCOL_VERSION: &str = "2025-06-18";
const DEFAULT_URL: &str = "http://host.docker.internal:8000";

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
                // Echo the client's protocolVersion — strict clients (agy) will
                // list tools but refuse tools/call if we advertise a version they
                // didn't offer. (Same lesson as nuts-files.)
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
    match dispatch(name, &args) {
        Ok(text) => ok(id, json!({ "content": [{ "type": "text", "text": text }] })),
        Err(e) => ok(
            id,
            json!({ "content": [{ "type": "text", "text": format!("error: {e}") }], "isError": true }),
        ),
    }
}

fn dispatch(name: &str, a: &Value) -> Result<String, String> {
    match name {
        "shivvr_embed" => shivvr_embed(a),
        "shivvr_similarity" => shivvr_similarity(a),
        "shivvr_status" => shivvr_status(a),
        _ => Err(format!("unknown tool: {name}")),
    }
}

// ── tool schema ───────────────────────────────────────────────────────────────

fn tool_list() -> Vec<Value> {
    let s = |name: &str, desc: &str, props: Value, required: Vec<&str>| {
        json!({
            "name": name,
            "description": desc,
            "inputSchema": { "type": "object", "properties": props, "required": required }
        })
    };
    let p_str = |d: &str| json!({ "type": "string", "description": d });
    let p_bool = |d: &str| json!({ "type": "boolean", "description": d });

    vec![
        s(
            "shivvr_similarity",
            "Semantic similarity between two texts via shivvr embeddings. Returns the cosine \
             similarity (-1..1; ~1 = near-identical meaning, ~0 = unrelated) plus a plain-language \
             reading. The agent-friendly way to ask 'do these mean the same thing?' without \
             handling raw vectors.",
            json!({
                "text_a": p_str("first text"),
                "text_b": p_str("second text"),
            }),
            vec!["text_a", "text_b"],
        ),
        s(
            "shivvr_embed",
            "Embed text into a vector via the local shivvr service (POST /memory/_mcp/ingest). \
             By default returns a summary (dimension + L2 norm + a few sample values) to keep \
             context small; set full=true to get the entire vector as a JSON array (e.g. to store \
             it). Use shivvr_similarity instead if you just want to compare two texts.",
            json!({
                "text": p_str("text to embed"),
                "full": p_bool("include the full embedding vector (default false → summary only)"),
            }),
            vec!["text"],
        ),
        s(
            "shivvr_status",
            "Report which shivvr endpoint this tool is pointed at (SHIVVR_URL), whether it's \
             reachable, and the embedding dimension. Call this first if embeds are failing.",
            json!({}),
            vec![],
        ),
    ]
}

// ── shivvr client ─────────────────────────────────────────────────────────────

/// Resolved shivvr base URL (env SHIVVR_URL, else the local default).
fn shivvr_url() -> String {
    std::env::var("SHIVVR_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_URL.to_string())
}

/// Embed `text` via shivvr, returning the embedding vector.
fn embed(text: &str) -> Result<Vec<f64>, String> {
    let base = shivvr_url();
    let url = format!("{}/memory/_mcp/ingest", base.trim_end_matches('/'));
    let body = json!({ "text": text }).to_string();
    let resp = ureq::post(&url)
        .set("Content-Type", "application/json")
        .timeout(Duration::from_secs(30))
        .send_string(&body)
        .map_err(|e| {
            format!("request to shivvr at {url} failed: {e} — is shivvr running locally? set SHIVVR_URL to point at it")
        })?;
    let txt = resp
        .into_string()
        .map_err(|e| format!("reading shivvr response: {e}"))?;
    let v: Value = serde_json::from_str(&txt).map_err(|e| format!("shivvr returned non-JSON: {e}"))?;

    // Arena ShivvrClient shape: top-level "embedding", else chunks[0].embedding.
    let arr = v
        .get("embedding")
        .and_then(|e| e.as_array())
        .or_else(|| {
            v.get("chunks")
                .and_then(|c| c.as_array())
                .and_then(|c| c.first())
                .and_then(|c0| c0.get("embedding"))
                .and_then(|e| e.as_array())
        })
        .ok_or_else(|| format!("no embedding in shivvr response: {}", truncate(&txt, 200)))?;

    arr.iter()
        .map(|x| x.as_f64().ok_or_else(|| "non-numeric value in embedding".to_string()))
        .collect()
}

fn l2_norm(v: &[f64]) -> f64 {
    v.iter().map(|x| x * x).sum::<f64>().sqrt()
}

fn cosine(a: &[f64], b: &[f64]) -> Result<f64, String> {
    if a.len() != b.len() {
        return Err(format!("dimension mismatch: {} vs {}", a.len(), b.len()));
    }
    let (na, nb) = (l2_norm(a), l2_norm(b));
    if na == 0.0 || nb == 0.0 {
        return Err("zero-norm embedding".to_string());
    }
    let dot: f64 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    Ok(dot / (na * nb))
}

// ── tools ─────────────────────────────────────────────────────────────────────

fn shivvr_embed(a: &Value) -> Result<String, String> {
    let text = sreq(a, "text")?;
    let full = a.get("full").and_then(|v| v.as_bool()).unwrap_or(false);
    let v = embed(&text)?;
    let dim = v.len();
    let norm = l2_norm(&v);
    if full {
        Ok(json!({ "dim": dim, "norm": norm, "embedding": v }).to_string())
    } else {
        let head: Vec<f64> = v.iter().take(4).map(|x| round6(*x)).collect();
        Ok(format!(
            "embedded ok — dim={dim}, L2 norm={norm:.4}, first values={head:?} … (set full=true for the whole vector)"
        ))
    }
}

fn shivvr_similarity(a: &Value) -> Result<String, String> {
    let ta = sreq(a, "text_a")?;
    let tb = sreq(a, "text_b")?;
    let va = embed(&ta)?;
    let vb = embed(&tb)?;
    let sim = cosine(&va, &vb)?;
    let reading = match sim {
        s if s >= 0.85 => "near-identical meaning",
        s if s >= 0.6 => "strongly related",
        s if s >= 0.35 => "loosely related",
        s if s >= 0.1 => "weakly related",
        _ => "unrelated",
    };
    Ok(format!("cosine similarity = {sim:.4} ({reading})"))
}

fn shivvr_status(_a: &Value) -> Result<String, String> {
    let url = shivvr_url();
    match embed("ping") {
        Ok(v) => Ok(format!(
            "shivvr reachable at {url} — embedding dimension = {}",
            v.len()
        )),
        Err(e) => Ok(format!("shivvr NOT reachable at {url}\n  {e}")),
    }
}

// ── helpers ─────────────────────────────────────────────────────────────────

fn sreq(a: &Value, k: &str) -> Result<String, String> {
    a.get(k)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("missing required string arg: {k}"))
}

fn round6(x: f64) -> f64 {
    (x * 1e6).round() / 1e6
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n).collect::<String>())
    }
}
