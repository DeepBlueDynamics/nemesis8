//! `n8 connect <provider>` — reach a backend n8 is running on ANOTHER machine.
//!
//! Pure gateway client (no Docker here): listen on `127.0.0.1:<port>` on this
//! machine and bridge each accepted TCP connection over a WebSocket to the
//! gateway (`GET /exposed/{host_port}/stream`, bearer-gated), which splices it
//! into the same container-side tunnel its own host-local forwarder uses. The
//! backend (Hermes `serve`, bound to container-loopback) sees a loopback peer
//! exactly as in the same-host case, so its token-only auth keeps working and
//! Hermes Desktop on this machine is configured with the printed URL + token.
//!
//! Wire contract (gateway side):
//! - `GET /exposed` → mappings `{ id, agent_id, internal_port, host_port, name,
//!   state: pending|live|degraded, attached_clients, provider, … }`
//! - `GET /exposed/{host_port}/stream` → WebSocket; Binary frames ⇄ TCP bytes;
//!   404 no mapping, 409 degraded/pending, close 1013 no client ready.
//! - `GET /serve-tokens/{provider}` → `{ provider, token }`; 404 if absent.

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header::AUTHORIZATION;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message;

/// One entry of the gateway's `GET /exposed`. Extra fields are tolerated.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Mapping {
    pub id: String,
    pub agent_id: String,
    pub internal_port: u16,
    pub host_port: u16,
    pub name: String,
    pub state: String,
    #[serde(default)]
    pub container_ref: Option<String>,
    #[serde(default)]
    pub tunnel_port: Option<u16>,
    #[serde(default)]
    pub attached_clients: usize,
    #[serde(default)]
    pub provider: Option<String>,
}

pub struct ConnectOpts {
    /// Provider whose backend to reach (matched by `provider`, else by
    /// `name == "<provider>-serve"`).
    pub provider: String,
    /// Gateway base URL, e.g. `http://host:9801`.
    pub remote: String,
    /// Gateway bearer token (None = open gateway).
    pub token: Option<String>,
    /// Local loopback port; defaults to the mapping's host port.
    pub local_port: Option<u16>,
}

/// Max bytes read from the local socket per WebSocket Binary frame.
const FRAME_MAX: usize = 16 * 1024;
/// How often the mapping is re-read from the gateway for state changes.
const POLL_EVERY: Duration = Duration::from_secs(10);
/// Repeated bridge failures of one kind are logged at most this often.
const LOG_EVERY: Duration = Duration::from_secs(10);

/// `http://…` → `ws://…`, `https://…` → `wss://…`; trailing slash dropped.
pub fn ws_base(remote: &str) -> String {
    let r = remote.trim().trim_end_matches('/');
    if let Some(rest) = r.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = r.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        r.to_string()
    }
}

/// The mapping for `provider`: by `provider` field, else by the
/// `"<provider>-serve"` name serve-backend gives its mapping. Prefers `live`.
pub fn select_mapping<'a>(maps: &'a [Mapping], provider: &str) -> Option<&'a Mapping> {
    let serve_name = format!("{provider}-serve");
    let matches = |m: &Mapping| {
        m.provider.as_deref() == Some(provider) || m.name == serve_name
    };
    maps.iter()
        .find(|m| matches(m) && m.state == "live")
        .or_else(|| maps.iter().find(|m| matches(m)))
}

fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(8))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

fn with_auth(req: reqwest::RequestBuilder, token: Option<&str>) -> reqwest::RequestBuilder {
    match token {
        Some(t) => req.bearer_auth(t),
        None => req,
    }
}

#[derive(Deserialize)]
struct Health {
    #[serde(default)]
    version: String,
}

#[derive(Deserialize)]
struct ServeToken {
    token: String,
}

/// Fetch `/exposed`. Err on transport failure; Ok(Err(status)) on a non-2xx.
async fn fetch_exposed(
    http: &reqwest::Client,
    remote: &str,
    token: Option<&str>,
) -> Result<std::result::Result<Vec<Mapping>, reqwest::StatusCode>> {
    let resp = with_auth(http.get(format!("{remote}/exposed")), token)
        .send()
        .await
        .with_context(|| format!("reaching the gateway at {remote}"))?;
    if !resp.status().is_success() {
        return Ok(Err(resp.status()));
    }
    let maps: Vec<Mapping> = resp.json().await.context("parsing /exposed")?;
    Ok(Ok(maps))
}

/// Log a bridge failure, but the same `reason` at most once per LOG_EVERY.
fn log_limited(reason: &str, detail: &str) {
    static LAST: OnceLock<Mutex<HashMap<String, Instant>>> = OnceLock::new();
    let map = LAST.get_or_init(|| Mutex::new(HashMap::new()));
    let now = Instant::now();
    let mut guard = match map.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    let due = guard
        .get(reason)
        .map(|t| now.duration_since(*t) >= LOG_EVERY)
        .unwrap_or(true);
    if due {
        guard.insert(reason.to_string(), now);
        eprintln!("bridge: {detail}");
    }
}

/// Bridge ONE accepted local connection over a fresh WebSocket to the gateway.
/// Returns when either side closes. Never panics on a bad peer.
pub async fn bridge_one(ws_url: String, token: Option<String>, mut tcp: TcpStream) {
    let mut request = match ws_url.as_str().into_client_request() {
        Ok(r) => r,
        Err(e) => {
            log_limited("bad-url", &format!("invalid gateway WebSocket URL {ws_url}: {e}"));
            return;
        }
    };
    if let Some(t) = token.as_deref() {
        if let Ok(v) = HeaderValue::from_str(&format!("Bearer {t}")) {
            request.headers_mut().insert(AUTHORIZATION, v);
        }
    }
    let (ws, _resp) = match tokio_tungstenite::connect_async(request).await {
        Ok(v) => v,
        Err(tokio_tungstenite::tungstenite::Error::Http(resp)) => {
            let status = resp.status();
            let why = match status.as_u16() {
                401 => "gateway rejected the token (401)".to_string(),
                404 => "no tunnel mapping on the gateway for this port (404)".to_string(),
                409 => "mapping is degraded/pending on the gateway (409) — backend down?".to_string(),
                s => format!("gateway refused the WebSocket (HTTP {s})"),
            };
            log_limited(&format!("http-{status}"), &format!("{why}; closing local connection"));
            let _ = tcp.shutdown().await;
            return;
        }
        Err(e) => {
            log_limited("connect", &format!("cannot open WebSocket to gateway: {e}; closing local connection"));
            let _ = tcp.shutdown().await;
            return;
        }
    };

    let (mut sink, mut stream) = ws.split();
    let (mut rd, mut wr) = tcp.into_split();
    // One writer owns the sink; both directions (and Pong replies) go through it.
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Message>(64);

    let writer = tokio::spawn(async move {
        while let Some(m) = rx.recv().await {
            let closing = matches!(m, Message::Close(_));
            if sink.send(m).await.is_err() || closing {
                break;
            }
        }
        let _ = sink.close().await;
    });

    // local TCP → WebSocket Binary frames
    let tx_up = tx.clone();
    let up = tokio::spawn(async move {
        let mut buf = vec![0u8; FRAME_MAX];
        loop {
            match rd.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx_up.send(Message::Binary(buf[..n].to_vec().into())).await.is_err() {
                        break;
                    }
                }
            }
        }
        let _ = tx_up.send(Message::Close(None)).await;
    });

    // WebSocket → local TCP
    let tx_down = tx;
    let down = tokio::spawn(async move {
        while let Some(item) = stream.next().await {
            match item {
                Ok(Message::Binary(b)) => {
                    if wr.write_all(&b).await.is_err() {
                        break;
                    }
                }
                Ok(Message::Ping(p)) => {
                    // The stream half can't write; route the Pong through the
                    // writer so the gateway's liveness pings are answered even
                    // when the local side is idle.
                    if tx_down.send(Message::Pong(p)).await.is_err() {
                        break;
                    }
                }
                Ok(Message::Close(frame)) => {
                    if let Some(f) = frame {
                        if u16::from(f.code) == 1013 {
                            log_limited("1013", "gateway has no container client ready (1013); closing local connection");
                        }
                    }
                    break;
                }
                Ok(_) => {} // Text / Pong / raw frames: ignore
                Err(_) => break,
            }
        }
        let _ = wr.shutdown().await;
    });

    // Whichever direction ends first tears the other down.
    tokio::select! {
        _ = up => {}
        _ = down => {}
    }
    // Dropping the remaining senders closes the writer; abort any straggler.
    writer.abort();
}

/// Run `n8 connect`. Returns the process exit code (0 on Ctrl-C).
pub async fn run(opts: ConnectOpts) -> Result<i32> {
    let remote = opts.remote.trim().trim_end_matches('/').to_string();
    let token = opts.token.clone();
    let http = http_client();

    // Gateway reachable? (/health is public.)
    let version = match http.get(format!("{remote}/health")).send().await {
        Ok(r) if r.status().is_success() => r
            .json::<Health>()
            .await
            .map(|h| h.version)
            .unwrap_or_default(),
        Ok(r) => {
            eprintln!("gateway at {remote} answered /health with HTTP {}", r.status());
            return Ok(2);
        }
        Err(e) => {
            eprintln!("cannot reach the gateway at {remote}: {e}");
            eprintln!("  is `n8 serve` running there, and is --remote / NEMESIS8_REMOTE right?");
            return Ok(2);
        }
    };

    // Which mapping?
    let maps = match fetch_exposed(&http, &remote, token.as_deref()).await? {
        Ok(m) => m,
        Err(status) if status.as_u16() == 401 => {
            eprintln!("gateway rejected the token (check --token / NEMESIS8_TOKEN)");
            return Ok(2);
        }
        Err(status) => {
            eprintln!("gateway answered /exposed with HTTP {status}");
            return Ok(2);
        }
    };
    let Some(mapping) = select_mapping(&maps, &opts.provider).cloned() else {
        eprintln!("no exposed backend for '{}' on {remote}", opts.provider);
        if maps.is_empty() {
            eprintln!("  nothing is exposed there — on the host run: n8 --provider {} serve-backend", opts.provider);
        } else {
            eprintln!("  exposed there:");
            for m in &maps {
                eprintln!(
                    "    {:<10} {:<22} host-port {:<6} {}  ({})",
                    m.provider.as_deref().unwrap_or("-"),
                    m.agent_id,
                    m.host_port,
                    m.state,
                    m.name
                );
            }
        }
        return Ok(2);
    };

    // Desktop token (optional).
    let desktop_token = match with_auth(
        http.get(format!("{remote}/serve-tokens/{}", opts.provider)),
        token.as_deref(),
    )
    .send()
    .await
    {
        Ok(r) if r.status().is_success() => r.json::<ServeToken>().await.ok().map(|t| t.token),
        Ok(r) if r.status().as_u16() == 404 => {
            eprintln!("warning: no desktop token on the gateway for {}", opts.provider);
            None
        }
        Ok(r) => {
            eprintln!("warning: /serve-tokens/{} answered HTTP {}", opts.provider, r.status());
            None
        }
        Err(e) => {
            eprintln!("warning: could not fetch the desktop token: {e}");
            None
        }
    };

    // Local listener.
    let local_port = opts.local_port.unwrap_or(mapping.host_port);
    let listener = match TcpListener::bind(("127.0.0.1", local_port)).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("cannot listen on 127.0.0.1:{local_port}: {e}");
            eprintln!(
                "  pick another with --local-port (Hermes Desktop's own local agent holds 9119)"
            );
            return Ok(2);
        }
    };

    println!("gateway        {remote:<26}ok (n8 {version})");
    println!(
        "mapping        {}  {}  host-port {}  {}",
        opts.provider, mapping.agent_id, mapping.host_port, mapping.state
    );
    println!("local          http://127.0.0.1:{local_port:<11}listening");
    if let Some(t) = &desktop_token {
        println!("desktop token  {t}");
        println!("               Hermes Desktop → remote gateway → the URL above + this token");
    }

    // Accept loop: never exits while we run; every connection gets its own WS.
    let ws_url = format!("{}/exposed/{}/stream", ws_base(&remote), mapping.host_port);
    let accept_token = token.clone();
    let accept = tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((tcp, _)) => {
                    let url = ws_url.clone();
                    let tok = accept_token.clone();
                    tokio::spawn(bridge_one(url, tok, tcp));
                }
                Err(e) => {
                    log_limited("accept", &format!("accept failed: {e}"));
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            }
        }
    });

    // Poll the mapping and report changes.
    let poll_http = http.clone();
    let poll_remote = remote.clone();
    let poll_token = token.clone();
    let poll_provider = opts.provider.clone();
    let mut last: Option<(String, usize)> = Some((mapping.state.clone(), mapping.attached_clients));
    let poll = tokio::spawn(async move {
        let mut tick = tokio::time::interval(POLL_EVERY);
        tick.tick().await; // first tick fires immediately; skip it
        loop {
            tick.tick().await;
            let now = match fetch_exposed(&poll_http, &poll_remote, poll_token.as_deref()).await {
                Ok(Ok(maps)) => select_mapping(&maps, &poll_provider)
                    .map(|m| (m.state.clone(), m.attached_clients)),
                Ok(Err(status)) => {
                    log_limited("poll-http", &format!("gateway /exposed answered HTTP {status}"));
                    continue;
                }
                Err(e) => {
                    log_limited("poll", &format!("gateway unreachable: {e}"));
                    continue;
                }
            };
            if now != last {
                match &now {
                    Some((state, attached)) => {
                        println!("mapping        {poll_provider}  {state}  ({attached} tunnel client(s) attached)")
                    }
                    None => println!("mapping        {poll_provider}  gone from the gateway — local connections will be refused until it's back"),
                }
                last = now;
            }
        }
    });

    tokio::signal::ctrl_c().await.context("waiting for Ctrl-C")?;
    accept.abort();
    poll.abort();
    println!("disconnected");
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(provider: Option<&str>, name: &str, state: &str, host_port: u16) -> Mapping {
        Mapping {
            id: format!("id-{host_port}"),
            agent_id: format!("n8-{name}"),
            internal_port: host_port,
            host_port,
            name: name.to_string(),
            state: state.to_string(),
            container_ref: None,
            tunnel_port: None,
            attached_clients: 0,
            provider: provider.map(str::to_string),
        }
    }

    #[test]
    fn ws_base_maps_schemes_and_trims_slash() {
        assert_eq!(ws_base("http://host:9801"), "ws://host:9801");
        assert_eq!(ws_base("https://host/"), "wss://host");
        assert_eq!(ws_base("http://127.0.0.1:9801/"), "ws://127.0.0.1:9801");
        assert_eq!(ws_base("ws://already"), "ws://already");
        assert_eq!(ws_base("  https://h:1/  "), "wss://h:1");
    }

    #[test]
    fn select_by_provider_then_name_prefers_live() {
        let maps = vec![
            m(Some("hermes"), "hermes-serve", "degraded", 8642),
            m(None, "hermes-serve", "live", 8643),
            m(Some("codex"), "codex-oauth", "live", 1455),
        ];
        // Prefers the live one even though the first also matches.
        assert_eq!(select_mapping(&maps, "hermes").unwrap().host_port, 8643);
        // By provider field.
        assert_eq!(select_mapping(&maps, "codex").unwrap().host_port, 1455);
        // Falls back to a non-live match when no live one exists.
        let only_degraded = vec![m(Some("hermes"), "hermes-serve", "degraded", 8642)];
        assert_eq!(select_mapping(&only_degraded, "hermes").unwrap().host_port, 8642);
        // Name fallback when provider is null.
        let by_name = vec![m(None, "pi-serve", "live", 9000)];
        assert_eq!(select_mapping(&by_name, "pi").unwrap().host_port, 9000);
        assert!(select_mapping(&maps, "nope").is_none());
    }

    #[tokio::test]
    async fn bridge_round_trips_bytes_through_an_echo_websocket() {
        // Echo WebSocket "gateway": Binary frames come straight back.
        let srv = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let srv_addr = srv.local_addr().unwrap();
        tokio::spawn(async move {
            let (s, _) = srv.accept().await.unwrap();
            let ws = tokio_tungstenite::accept_async(s).await.unwrap();
            let (mut sink, mut stream) = ws.split();
            while let Some(Ok(msg)) = stream.next().await {
                match msg {
                    Message::Binary(b) => {
                        if sink.send(Message::Binary(b)).await.is_err() {
                            break;
                        }
                    }
                    Message::Close(_) => break,
                    _ => {}
                }
            }
        });

        // Local listener → bridge_one for the first connection.
        let local = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let local_port = local.local_addr().unwrap().port();
        let ws_url = format!("ws://{srv_addr}/exposed/{local_port}/stream");
        tokio::spawn(async move {
            let (tcp, _) = local.accept().await.unwrap();
            bridge_one(ws_url, None, tcp).await;
        });

        let mut client = TcpStream::connect(("127.0.0.1", local_port)).await.unwrap();
        client.write_all(b"hello through the bridge").await.unwrap();
        let mut buf = vec![0u8; 64];
        let n = tokio::time::timeout(Duration::from_secs(5), client.read(&mut buf))
            .await
            .expect("echo within 5s")
            .unwrap();
        assert_eq!(&buf[..n], b"hello through the bridge");

        // Closing the local side ends the bridge cleanly (no hang).
        drop(client);
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
