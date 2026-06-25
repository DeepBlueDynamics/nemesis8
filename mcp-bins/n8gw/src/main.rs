//! n8gw — a native MCP client for the nemesis8 gateway / control plane.
//!
//! Gives agents running inside an n8 container a way to talk to the host gateway
//! (`n8 serve`) — list/spawn/kill agents, list triggers, check status — over its
//! REST API. Speaks the MCP stdio transport directly (newline-delimited JSON-RPC
//! 2.0), same hand-rolled pattern as nuts-files/shivvr/ask. The Rust replacement
//! for nemesis8-orchestrator.py (instant cold-start, no Python).
//!
//! KEY BEHAVIOR: if the gateway isn't running, tools DON'T error or blow up — they
//! return a calm, plain-text "gateway not running, start it with `n8 serve
//! --background`" so the agent can carry on instead of choking on a stack trace.
//!
//! Env (forwarded by n8's build_env):
//!   GATEWAY_URL          — default http://host.docker.internal:4000
//!   NEMESIS8_AUTH_TOKEN  — optional Bearer token if the gateway requires auth

use serde_json::{json, Value};
use std::io::{self, BufRead, Write};
use std::time::Duration;

const SERVER_NAME: &str = "nemesis8";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");
const PROTOCOL_VERSION: &str = "2025-06-18";
const DEFAULT_GATEWAY: &str = "http://host.docker.internal:4000";

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
                // didn't offer. (Same lesson as nuts-files/shivvr/ask.)
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
        "gateway_status" => gateway_status(),
        "agent_list" => agent_list(),
        "agent_spawn" => agent_spawn(&args),
        "agent_kill" => agent_kill(&args),
        "trigger_list" => trigger_list(),
        "expose_port" => expose_port(&args),
        "unexpose_port" => unexpose_port(&args),
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
    let obj = |props: Value, required: Value| json!({ "type": "object", "properties": props, "required": required });
    vec![
        json!({
            "name": "gateway_status",
            "description": "Check whether the nemesis8 gateway/control-plane is running, and if so its \
                active runs, scheduler state, agent count, and uptime. Never errors when the gateway is \
                down — it just reports that it's not running and how to start it.",
            "inputSchema": obj(json!({}), json!([]))
        }),
        json!({
            "name": "agent_list",
            "description": "List every nemesis8 agent the control plane knows about (running, idle, \
                exited), including hand-started containers it discovered. Returns gracefully if the \
                gateway isn't running.",
            "inputSchema": obj(json!({}), json!([]))
        }),
        json!({
            "name": "agent_spawn",
            "description": "Spawn a new nemesis8 agent via the gateway to work a prompt in a container. \
                Returns its id. Use agent_list to see it once the next reconcile tick picks it up.",
            "inputSchema": obj(
                json!({
                    "prompt": p_str("the task/prompt for the new agent"),
                    "provider": p_str("optional provider (codex, claude, gemini, pi, …); gateway default if omitted"),
                    "model": p_str("optional model id"),
                    "workspace": p_str("optional host workspace path to mount"),
                }),
                json!(["prompt"])
            )
        }),
        json!({
            "name": "agent_kill",
            "description": "Stop a running agent by its id (from agent_list). The session is preserved; \
                only the container is killed.",
            "inputSchema": obj(json!({ "id": p_str("agent id to kill") }), json!(["id"]))
        }),
        json!({
            "name": "trigger_list",
            "description": "List the gateway's scheduled triggers (cron-style prompts), with each one's \
                schedule and last-fired status. Returns gracefully if the gateway isn't running.",
            "inputSchema": obj(json!({}), json!([]))
        }),
        json!({
            "name": "expose_port",
            "description": "Expose a TCP server running inside this agent container to the host over the \
                nemesis8 reverse tunnel. Returns a host-loopback URL such as http://127.0.0.1:18042.",
            "inputSchema": obj(
                json!({
                    "port": json!({ "type": "integer", "description": "container-local TCP port to expose" }),
                    "name": p_str("optional label for the exposed port"),
                }),
                json!(["port"])
            )
        }),
        json!({
            "name": "unexpose_port",
            "description": "Stop and release a previous expose_port mapping by id.",
            "inputSchema": obj(json!({ "id": p_str("mapping id returned by expose_port") }), json!(["id"]))
        }),
    ]
}

// ── tools ──────────────────────────────────────────────────────────────────

fn gateway_status() -> Result<String, String> {
    match gw_request("GET", "/status", None) {
        Ok(v) => Ok(format!(
            "gateway running at {}\n\n{}",
            gw_base(),
            pretty(&v)
        )),
        Err(GwError::Down) => Ok(down_message()),
        Err(GwError::Failed(e)) => Err(e),
    }
}

fn agent_list() -> Result<String, String> {
    match gw_request("GET", "/agents", None) {
        Ok(v) => Ok(pretty(&v)),
        Err(GwError::Down) => Ok(down_message()),
        Err(GwError::Failed(e)) => Err(e),
    }
}

fn agent_spawn(a: &Value) -> Result<String, String> {
    let prompt = sreq(a, "prompt")?;
    let mut body = json!({ "prompt": prompt });
    for k in ["provider", "model", "workspace"] {
        if let Some(s) = a.get(k).and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
            body[k] = json!(s);
        }
    }
    match gw_request("POST", "/agents/spawn", Some(&body)) {
        Ok(v) => Ok(pretty(&v)),
        Err(GwError::Down) => Ok(down_message()),
        Err(GwError::Failed(e)) => Err(e),
    }
}

fn agent_kill(a: &Value) -> Result<String, String> {
    let id = sreq(a, "id")?;
    // The gateway routes by global id (host/local); pass it through verbatim.
    let path = format!("/agents/{}/kill", urlish(&id));
    match gw_request("POST", &path, Some(&json!({}))) {
        Ok(v) => Ok(pretty(&v)),
        Err(GwError::Down) => Ok(down_message()),
        Err(GwError::Failed(e)) => Err(e),
    }
}

fn trigger_list() -> Result<String, String> {
    match gw_request("GET", "/triggers", None) {
        Ok(v) => Ok(pretty(&v)),
        Err(GwError::Down) => Ok(down_message()),
        Err(GwError::Failed(e)) => Err(e),
    }
}

fn expose_port(a: &Value) -> Result<String, String> {
    let port = a
        .get("port")
        .and_then(|v| v.as_u64())
        .filter(|p| *p > 0 && *p <= u16::MAX as u64)
        .ok_or_else(|| "missing required integer arg: port".to_string())?;
    let agent_id = resolve_self_agent_id()?;
    let mut body = json!({ "agent_id": agent_id, "port": port as u16 });
    if let Some(name) = a
        .get("name")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
    {
        body["name"] = json!(name);
    }
    match gw_request("POST", "/expose", Some(&body)) {
        Ok(v) => Ok(pretty(&v)),
        Err(GwError::Down) => Ok(down_message()),
        Err(GwError::Failed(e)) => Err(e),
    }
}

fn resolve_self_agent_id() -> Result<String, String> {
    if let Some(id) = std::env::var("NEMESIS8_AGENT_ID")
        .ok()
        .filter(|s| !s.trim().is_empty())
    {
        return Ok(id);
    }

    if let Some(id) =
        current_container_id().and_then(|container_id| match gw_request("GET", "/agents", None) {
            Ok(Value::Array(agents)) => agents.into_iter().find_map(|agent| {
                let cid = agent.get("container_id")?.as_str()?;
                if cid == container_id
                    || cid.starts_with(&container_id)
                    || container_id.starts_with(cid)
                {
                    agent
                        .get("local_id")
                        .and_then(|v| v.as_str())
                        .or_else(|| agent.get("id").and_then(|v| v.as_str()))
                        .map(|s| s.to_string())
                } else {
                    None
                }
            }),
            _ => None,
        })
    {
        return Ok(id);
    }

    std::env::var("HOSTNAME")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| {
            "NEMESIS8_AGENT_ID is not set and this container was not found in /agents".to_string()
        })
}

fn current_container_id() -> Option<String> {
    for path in ["/proc/self/mountinfo", "/proc/self/cgroup"] {
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        for token in text.split(|c: char| !c.is_ascii_hexdigit()) {
            if token.len() == 64 {
                return Some(token.to_string());
            }
        }
    }
    None
}

fn unexpose_port(a: &Value) -> Result<String, String> {
    let id = sreq(a, "id")?;
    match gw_request("POST", "/unexpose", Some(&json!({ "id": id }))) {
        Ok(v) => Ok(pretty(&v)),
        Err(GwError::Down) => Ok(down_message()),
        Err(GwError::Failed(e)) => Err(e),
    }
}

// ── gateway HTTP (blocking ureq) ─────────────────────────────────────────────

enum GwError {
    /// The gateway isn't reachable (refused / DNS / timeout) — i.e. not running.
    Down,
    /// Reached the gateway but it returned an error (HTTP status / bad body).
    Failed(String),
}

fn gw_base() -> String {
    std::env::var("GATEWAY_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_GATEWAY.to_string())
        .trim_end_matches('/')
        .to_string()
}

fn down_message() -> String {
    format!(
        "The nemesis8 gateway is not running at {} — so there's nothing to manage yet (no error, \
         just not up). Start it with `n8 serve --background`, or from the control room: Gateway ▸ \
         Start. Set GATEWAY_URL if it's on a different host/port.",
        gw_base()
    )
}

fn gw_request(method: &str, path: &str, body: Option<&Value>) -> Result<Value, GwError> {
    let url = format!("{}{}", gw_base(), path);
    let mut req = match method {
        "POST" => ureq::post(&url),
        _ => ureq::get(&url),
    }
    .timeout(Duration::from_secs(15));
    if let Ok(token) = std::env::var("NEMESIS8_AUTH_TOKEN") {
        if !token.trim().is_empty() {
            req = req.set("Authorization", &format!("Bearer {token}"));
        }
    }
    let sent = match body {
        Some(b) => req
            .set("Content-Type", "application/json")
            .send_string(&b.to_string()),
        None => req.call(),
    };
    match sent {
        Ok(resp) => {
            let txt = resp
                .into_string()
                .map_err(|e| GwError::Failed(format!("reading response: {e}")))?;
            if txt.trim().is_empty() {
                return Ok(json!({ "ok": true }));
            }
            serde_json::from_str(&txt)
                .map_err(|e| GwError::Failed(format!("non-JSON response: {e}")))
        }
        // HTTP status reached the gateway → a real failure, surface it.
        Err(ureq::Error::Status(code, resp)) => {
            let txt = resp.into_string().unwrap_or_default();
            Err(GwError::Failed(format!(
                "HTTP {code}: {}",
                truncate(&txt, 400)
            )))
        }
        // Transport error = couldn't reach the gateway = it's down. The whole point.
        Err(ureq::Error::Transport(_)) => Err(GwError::Down),
    }
}

// ── helpers ──────────────────────────────────────────────────────────────────

fn pretty(v: &Value) -> String {
    serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string())
}

fn sreq(a: &Value, k: &str) -> Result<String, String> {
    a.get(k)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("missing required string arg: {k}"))
}

/// Minimal path-segment guard: keep an id usable in a URL path without pulling in
/// a urlencoding crate. Agent ids are `host/local` slugs, so only spaces matter.
fn urlish(s: &str) -> String {
    s.trim().replace(' ', "%20")
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n).collect::<String>())
    }
}
