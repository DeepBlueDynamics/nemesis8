//! Hyperia identity minting — shared by the bin (process-level upgrade) and
//! `docker.rs` (per-agent token at container launch).
//!
//! Why per-agent: Hyperia keys tokens by NAME (same name → same token). A
//! workspace-keyed identity (`nemesis8/<workspace>`) therefore collapses every
//! agent in a workspace onto ONE token, so their sends are indistinguishable and
//! a drive-access grant to one lands on all (the #104 shared-identity bug: two
//! concurrent agents provisioned in one workspace both authenticated as the same
//! pane). Keying by the container's unique id gives each agent its own persistent
//! identity, which is what attribution actually needs.

/// Lowercase, ascii-alphanumeric-and-dash only — a workspace/container name can
/// contain anything; the identity name shouldn't.
pub fn sanitize_identity_segment(raw: &str) -> String {
    let s: String = raw
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    s.trim_matches('-').to_string()
}

/// Per-AGENT Hyperia identity name: `nemesis8/<workspace>/<agent>`, falling back
/// to `nemesis8/<agent>` or `nemesis8`. Each distinct agent id yields a distinct
/// name → a distinct persistent token → correct per-agent attribution.
pub fn agent_identity_name(workspace_basename: Option<&str>, agent_id: &str) -> String {
    let ws = workspace_basename
        .map(sanitize_identity_segment)
        .filter(|s| !s.is_empty());
    let agent = sanitize_identity_segment(agent_id);
    match (ws, agent.is_empty()) {
        (Some(ws), false) => format!("nemesis8/{ws}/{agent}"),
        (Some(ws), true) => format!("nemesis8/{ws}"),
        (None, false) => format!("nemesis8/{agent}"),
        (None, true) => "nemesis8".to_string(),
    }
}

/// Pull the first `hyp_agent_…` token out of a request_token response.
/// Schema-free: accepts plain-JSON and SSE-framed (`data: {…}`) streamable-HTTP
/// bodies alike, since the token is embedded in prose text content either way.
pub fn extract_hyp_agent_token(body: &str) -> Option<String> {
    let idx = body.find("hyp_agent_")?;
    let token: String = body[idx..]
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect();
    (token.len() > "hyp_agent_".len()).then_some(token)
}

/// Mint (or fetch — Hyperia returns the same token for the same name) a
/// persistent `hyp_agent_…` token for `identity_name` from the loopback sidecar.
/// `current_auth` is the caller's existing token, used to authenticate the mint
/// while it's still valid (request_token also answers unauthenticated on
/// loopback). Returns None if Hyperia is unreachable — callers fall back to
/// whatever token they already have.
pub fn mint_agent_token(identity_name: &str, current_auth: Option<&str>) -> Option<String> {
    // `reqwest::blocking` builds and drops its own tokio runtime. This function
    // is called from `check_integrations`, which runs inside n8's async
    // `#[tokio::main]` — dropping that runtime there panics with "Cannot drop a
    // runtime in a context where blocking is not allowed". Run the blocking HTTP
    // on a dedicated OS thread, which has no ambient tokio runtime. (Blocking the
    // caller on join is the same synchronous behavior the old code had.)
    let identity_name = identity_name.to_string();
    let current_auth = current_auth.map(str::to_string);
    std::thread::spawn(move || mint_agent_token_blocking(&identity_name, current_auth.as_deref()))
        .join()
        .ok()
        .flatten()
}

/// The blocking mint. MUST run on a thread with no ambient tokio runtime — see
/// [`mint_agent_token`], which is the only caller and provides that thread.
fn mint_agent_token_blocking(identity_name: &str, current_auth: Option<&str>) -> Option<String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_millis(2500))
        .build()
        .ok()?;
    let body = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {"name": "request_token", "arguments": {"name": identity_name}}
    });
    let mut req = client
        .post("http://127.0.0.1:9800/mcp")
        .header("Accept", "application/json, text/event-stream")
        .json(&body);
    if let Some(tok) = current_auth.map(str::trim).filter(|t| !t.is_empty()) {
        req = req.bearer_auth(tok);
    }
    let text = req.send().ok()?.text().ok()?;
    extract_hyp_agent_token(&text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_strips_non_alnum() {
        assert_eq!(sanitize_identity_segment("My Repo/2!"), "my-repo-2");
        assert_eq!(sanitize_identity_segment("---x---"), "x");
    }

    #[test]
    fn extract_from_sse_framed() {
        let body = "event: message\ndata: {\"text\":\"here: hyp_agent_abc123DEF456 done\"}\n";
        assert_eq!(extract_hyp_agent_token(body).as_deref(), Some("hyp_agent_abc123DEF456"));
        assert_eq!(extract_hyp_agent_token("no token here"), None);
    }

    #[test]
    fn agent_identity_is_unique_per_agent() {
        // Two agents in the same workspace get DISTINCT names → distinct tokens.
        let a = agent_identity_name(Some("research"), "n8-friendly-puma");
        let b = agent_identity_name(Some("research"), "n8-toxic-emu");
        assert_ne!(a, b);
        assert_eq!(a, "nemesis8/research/n8-friendly-puma");
        // Fallbacks.
        assert_eq!(agent_identity_name(None, "n8-x"), "nemesis8/n8-x");
        assert_eq!(agent_identity_name(Some("ws"), ""), "nemesis8/ws");
        assert_eq!(agent_identity_name(None, ""), "nemesis8");
    }

    #[tokio::test]
    async fn mint_from_async_context_does_not_panic() {
        // The regression: `reqwest::blocking` inside a tokio runtime panicked on
        // runtime drop. Hyperia is likely unreachable here (nothing on 9800) →
        // returns None fast; the point is that it must NOT panic in an async ctx.
        let _ = mint_agent_token("nemesis8/__test__", None);
    }
}
