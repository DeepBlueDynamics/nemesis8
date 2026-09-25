//! Hyperia identity minting — shared by the bin (process-level upgrade) and
//! `docker.rs` (per-agent token at container launch).
//!
//! Why per-agent: Hyperia keys identities by NAME. A workspace-keyed identity
//! (`nemesis8/<workspace>`) therefore collapses every agent in a workspace onto
//! ONE token, so their sends are indistinguishable and a drive-access grant to
//! one lands on all (the #104 shared-identity bug: two concurrent agents
//! provisioned in one workspace both authenticated as the same pane). Keying by
//! the container's name gives each agent its own persistent identity, which is
//! what attribution actually needs.
//!
//! Names are permanent on Hyperia's side, and since Hyperia a7ff1005 an existing
//! identity's token is re-issued only to a caller presenting that identity's
//! credential — `request_token` for a taken name answers "Identity already
//! exists". So a mint has three outcomes (minted / name taken / unreachable),
//! and `docker.rs` decides what a taken name means: reuse the credential this
//! host stored for it, or draw another name. It never silently keeps the pane
//! token that was in the env.

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

/// Why a mint handed back no token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MintError {
    /// `identity_name` is already registered and Hyperia will not re-issue its
    /// token by name (a7ff1005): present its credential, or use another name.
    NameTaken,
    /// The sidecar did not answer (Hyperia not running, or timed out).
    Unreachable,
    /// The sidecar answered with neither a token nor a recognised error.
    Other(String),
}

impl std::fmt::Display for MintError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MintError::NameTaken => write!(f, "identity name already registered"),
            MintError::Unreachable => write!(f, "Hyperia sidecar unreachable"),
            MintError::Other(s) => write!(f, "unexpected reply: {s}"),
        }
    }
}

/// Mint a persistent `hyp_agent_…` token for a NEW `identity_name` from the
/// loopback sidecar. `current_auth` is the caller's existing token, used to
/// authenticate the mint while it's still valid (request_token also answers
/// unauthenticated on loopback). A name that is already registered comes back
/// as [`MintError::NameTaken`]; the caller decides whether it holds that
/// identity's credential or must choose another name.
pub fn mint_agent_token(identity_name: &str, current_auth: Option<&str>) -> Result<String, MintError> {
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
        .unwrap_or(Err(MintError::Unreachable))
}

/// The blocking mint. MUST run on a thread with no ambient tokio runtime — see
/// [`mint_agent_token`], which is the only caller and provides that thread.
fn mint_agent_token_blocking(identity_name: &str, current_auth: Option<&str>) -> Result<String, MintError> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_millis(2500))
        .build()
        .map_err(|_| MintError::Unreachable)?;
    // single_session: a container IS its agent and runs exactly one Hyperia MCP
    // client, so Hyperia must not split it into per-session child principals
    // (mail addressed to the agent landed in a parent inbox no session read;
    // pane_bind from a child was refused). Sidecars without the flag ignore the
    // field and apply their interim rule for names under `nemesis8/`.
    let body = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {"name": "request_token", "arguments": {"name": identity_name, "single_session": true}}
    });
    let mut req = client
        .post("http://127.0.0.1:9800/mcp")
        .header("Accept", "application/json, text/event-stream")
        .json(&body);
    if let Some(tok) = current_auth.map(str::trim).filter(|t| !t.is_empty()) {
        req = req.bearer_auth(tok);
    }
    let text = req
        .send()
        .map_err(|_| MintError::Unreachable)?
        .text()
        .map_err(|_| MintError::Unreachable)?;
    classify_mint_response(&text)
}

/// Read a `request_token` reply. Hyperia answers inside MCP text content (plain
/// JSON or SSE `data:` frames, with the inner JSON escaped): a `hyp_agent_…`
/// token on success, or a refusal. Newer sidecars carry a stable code —
/// `{"ok":false,"code":"identity_exists","error":"Identity already exists; …"}`
/// — and older ones only the prose, so both are matched.
pub fn classify_mint_response(text: &str) -> Result<String, MintError> {
    if let Some(tok) = extract_hyp_agent_token(text) {
        return Ok(tok);
    }
    // Drop the escaping and whitespace so `\"code\":\"identity_exists\"` inside
    // MCP text content matches the same way as the bare JSON.
    let flat: String = text
        .chars()
        .filter(|c| *c != '\\' && !c.is_whitespace())
        .collect::<String>()
        .to_ascii_lowercase();
    if flat.contains("\"code\":\"identity_exists\"")
        || flat.contains("identityalreadyexists")
        || flat.contains("alreadyregistered")
        || flat.contains("nametaken")
    {
        return Err(MintError::NameTaken);
    }
    let brief: String = text.chars().filter(|c| !c.is_control()).take(160).collect();
    Err(MintError::Other(if brief.trim().is_empty() {
        "empty reply".to_string()
    } else {
        brief
    }))
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

    #[test]
    fn classify_token_taken_and_garbage() {
        let sse = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"Token: hyp_agent_Zz9\"}]}}\n";
        assert_eq!(classify_mint_response(sse), Ok("hyp_agent_Zz9".to_string()));
        // The exact reply Hyperia gives for a registered name (a7ff1005+).
        let taken = "data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"{\\\"error\\\":\\\"Identity already exists; present its credential.\\\",\\\"ok\\\":false}\"}],\"isError\":true}}\n";
        assert_eq!(classify_mint_response(taken), Err(MintError::NameTaken));
        // Newer sidecars add a stable code; match it even when the prose changes.
        let coded = "data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"{\\\"ok\\\":false,\\\"code\\\":\\\"identity_exists\\\",\\\"error\\\":\\\"nope\\\"}\"}],\"isError\":true}}\n";
        assert_eq!(classify_mint_response(coded), Err(MintError::NameTaken));
        // Other refusal codes are not "taken".
        let reserved = "{\"ok\":false,\"code\":\"name_reserved_by_session\",\"error\":\"reserved\"}";
        assert!(matches!(classify_mint_response(reserved), Err(MintError::Other(_))));
        assert!(matches!(classify_mint_response("<html>502 Bad Gateway</html>"), Err(MintError::Other(_))));
        assert_eq!(classify_mint_response("   "), Err(MintError::Other("empty reply".to_string())));
    }

    #[tokio::test]
    async fn mint_from_async_context_does_not_panic() {
        // The regression: `reqwest::blocking` inside a tokio runtime panicked on
        // runtime drop. The point is that it must NOT panic in an async ctx;
        // with no sidecar on 9800 it answers Unreachable fast, and with a live
        // one the throwaway name is either minted or reported taken.
        let _ = mint_agent_token("nemesis8/__test__", None);
    }
}
