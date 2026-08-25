//! Runtime port exposure: host loopback listeners paired with outbound
//! container clients on the sibling tunnel port.
//!
//! Protocol (`N8TUNNEL/1`): the container dials the gateway, sends one line
//! `N8TUNNEL/1 <host_port> <internal_port>\n`, the gateway replies
//! `N8TUNNEL/1 OK\n`, then both sides `copy_bidirectional` raw TCP. Each host
//! connection consumes one client connection; the client reconnects after the
//! stream ends (no multiplex framing).

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt, copy_bidirectional};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, oneshot};
use tokio::task::AbortHandle;

const PORT_RANGE_START: u16 = 18000;
const PORT_RANGE_END: u16 = 18999;
// OFFSET 2, NOT 1: gateway+1 (9802) is the trainer API. 9803 is the tunnel
// acceptor (container-outbound) in this port family.
pub const DEFAULT_TUNNEL_PORT_OFFSET: u16 = 2;

/// How many `--tunnel-client` workers `/expose` starts (concurrent streams).
pub const TUNNEL_CLIENT_WORKERS: usize = 2;

pub const PROTO_VERSION: &str = "N8TUNNEL/1";

/// Find a free TCP port in the tunnel range by bind-testing on host loopback.
pub fn allocate_port(used: &HashSet<u16>) -> Option<u16> {
    for port in PORT_RANGE_START..=PORT_RANGE_END {
        if used.contains(&port) {
            continue;
        }
        if let Ok(listener) = std::net::TcpListener::bind(("127.0.0.1", port)) {
            drop(listener);
            return Some(port);
        }
    }
    None
}

pub fn allocate_reserved_port(used: &HashSet<u16>) -> Option<u16> {
    (PORT_RANGE_START..=PORT_RANGE_END).find(|port| !used.contains(port))
}

/// Allocate an exact host port (OAuth callbacks) or a dynamic port in the
/// tunnel range. Exact ports are refused when already reserved or already
/// accepting connections on loopback.
pub fn allocate_host_port(
    used: &HashSet<u16>,
    requested: Option<u16>,
    skip_connect_probe: bool,
) -> Result<u16, HostPortError> {
    if let Some(requested) = requested {
        if requested == 0 || used.contains(&requested) || (!skip_connect_probe && port_accepts(requested))
        {
            return Err(HostPortError::ExactBusy(requested));
        }
        return Ok(requested);
    }
    let allocated = if skip_connect_probe {
        allocate_reserved_port(used)
    } else {
        allocate_port(used)
    };
    allocated.ok_or(HostPortError::RangeExhausted)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostPortError {
    ExactBusy(u16),
    RangeExhausted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TunnelDisableReason {
    PortOccupied { port: u16 },
    StartFailed { detail: String },
}

impl TunnelDisableReason {
    pub fn message(&self) -> String {
        match self {
            Self::PortOccupied { port } => format!(
                "reverse-tunnel plane is disabled: tunnel port {port} is occupied by another listener — free that port or run `n8 --port <other>` so the sibling tunnel port moves, then restart `n8 serve`"
            ),
            Self::StartFailed { detail } => format!(
                "reverse-tunnel plane is disabled: tunnel acceptor failed to bind ({detail}) — restart `n8 serve` or publish callback ports at launch with --publish"
            ),
        }
    }
}

#[derive(Serialize, Clone)]
pub struct PortMapping {
    pub id: String,
    pub agent_id: String,
    pub internal_port: u16,
    pub host_port: u16,
    pub name: String,
    pub state: MappingState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container_ref: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tunnel_port: Option<u16>,
}

#[derive(Serialize, Clone, Copy)]
#[serde(rename_all = "lowercase")]
pub enum MappingState {
    Pending,
    Live,
}

/// Registry of allocated port mappings (control plane).
pub struct TunnelRegistry {
    pub mappings: HashMap<String, PortMapping>,
}

impl TunnelRegistry {
    pub fn new() -> Self {
        Self {
            mappings: HashMap::new(),
        }
    }

    pub fn used_ports(&self) -> HashSet<u16> {
        self.mappings.values().map(|m| m.host_port).collect()
    }
}

impl Default for TunnelRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Deserialize)]
pub struct ExposeRequest {
    pub agent_id: String,
    pub port: u16,
    pub name: Option<String>,
    /// Request an exact host port instead of allocating from the dynamic range.
    /// Used for OAuth providers whose redirect URI is fixed to localhost.
    #[serde(default)]
    pub host_port: Option<u16>,
}

#[derive(Serialize)]
pub struct ExposeResponse {
    pub id: String,
    pub public_url: String,
    pub host_port: u16,
}

#[derive(Deserialize)]
pub struct UnexposeRequest {
    pub id: String,
}

pub fn sibling_tunnel_port(gateway_port: u16) -> u16 {
    gateway_port.saturating_add(DEFAULT_TUNNEL_PORT_OFFSET)
}

pub fn port_accepts(port: u16) -> bool {
    std::net::TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        Duration::from_millis(250),
    )
    .is_ok()
}

pub async fn wait_for_port(port: u16, timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if port_accepts(port) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

/// Union of `[provider.login].callback_ports` across the provider registry.
pub fn declared_callback_ports() -> Vec<u16> {
    let mut set = HashSet::new();
    for def in crate::provider_registry::ProviderRegistry::load().all() {
        for port in &def.provider.login.callback_ports {
            if *port != 0 {
                set.insert(*port);
            }
        }
    }
    let mut ports: Vec<u16> = set.into_iter().collect();
    ports.sort_unstable();
    ports
}

pub fn encode_hello(host_port: u16, internal_port: u16) -> String {
    format!("{PROTO_VERSION} {host_port} {internal_port}\n")
}

pub fn encode_ok() -> String {
    format!("{PROTO_VERSION} OK\n")
}

pub fn encode_err(msg: &str) -> String {
    format!("{PROTO_VERSION} ERR {msg}\n")
}

pub fn parse_hello(line: &str) -> Result<(u16, u16), String> {
    let line = line.trim();
    let mut parts = line.split_whitespace();
    match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some(PROTO_VERSION), Some(h), Some(i), None) => {
            let host_port = h.parse().map_err(|_| format!("bad host_port {h}"))?;
            let internal_port = i.parse().map_err(|_| format!("bad internal_port {i}"))?;
            Ok((host_port, internal_port))
        }
        _ => Err(format!("expected `{PROTO_VERSION} <host_port> <internal_port>`")),
    }
}

pub fn parse_reply(line: &str) -> Result<(), String> {
    let line = line.trim();
    if line == format!("{PROTO_VERSION} OK") {
        return Ok(());
    }
    if let Some(rest) = line.strip_prefix(&format!("{PROTO_VERSION} ERR ")) {
        return Err(rest.to_string());
    }
    Err(format!("unexpected reply {line:?}"))
}

async fn read_line(stream: &mut TcpStream) -> Result<String> {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        stream
            .read_exact(&mut byte)
            .await
            .context("reading handshake line")?;
        if byte[0] == b'\n' {
            break;
        }
        buf.push(byte[0]);
        if buf.len() > 256 {
            anyhow::bail!("handshake line too long");
        }
    }
    Ok(String::from_utf8_lossy(&buf)
        .trim_end_matches('\r')
        .to_string())
}

/// Live data-plane: idle container clients + host listeners.
pub struct TunnelHub {
    live: Mutex<HashSet<u16>>,
    idle: Mutex<HashMap<u16, Vec<TcpStream>>>,
    waiters: Mutex<HashMap<u16, Vec<oneshot::Sender<TcpStream>>>>,
    jobs: Mutex<HashMap<String, Vec<AbortHandle>>>,
}

impl TunnelHub {
    pub fn new() -> Self {
        Self {
            live: Mutex::new(HashSet::new()),
            idle: Mutex::new(HashMap::new()),
            waiters: Mutex::new(HashMap::new()),
            jobs: Mutex::new(HashMap::new()),
        }
    }

    pub async fn open_port(&self, host_port: u16) {
        self.live.lock().await.insert(host_port);
    }

    pub async fn is_live(&self, host_port: u16) -> bool {
        self.live.lock().await.contains(&host_port)
    }

    pub async fn offer_client(&self, host_port: u16, mut stream: TcpStream) {
        {
            let mut waiters = self.waiters.lock().await;
            if let Some(list) = waiters.get_mut(&host_port) {
                while let Some(tx) = list.pop() {
                    match tx.send(stream) {
                        Ok(()) => return,
                        Err(s) => stream = s,
                    }
                }
            }
        }
        self.idle
            .lock()
            .await
            .entry(host_port)
            .or_default()
            .push(stream);
    }

    pub async fn take_client(&self, host_port: u16, timeout: Duration) -> Option<TcpStream> {
        {
            let mut idle = self.idle.lock().await;
            if let Some(v) = idle.get_mut(&host_port) {
                if let Some(s) = v.pop() {
                    return Some(s);
                }
            }
        }
        let (tx, rx) = oneshot::channel();
        self.waiters
            .lock()
            .await
            .entry(host_port)
            .or_default()
            .push(tx);
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(s)) => Some(s),
            _ => None,
        }
    }

    pub async fn track_job(&self, id: &str, handle: AbortHandle) {
        self.jobs
            .lock()
            .await
            .entry(id.to_string())
            .or_default()
            .push(handle);
    }

    pub async fn close_mapping(&self, id: &str, host_port: u16) {
        self.live.lock().await.remove(&host_port);
        if let Some(handles) = self.jobs.lock().await.remove(id) {
            for h in handles {
                h.abort();
            }
        }
        let idle = self.idle.lock().await.remove(&host_port).unwrap_or_default();
        drop(idle);
        self.waiters.lock().await.remove(&host_port);
    }
}

impl Default for TunnelHub {
    fn default() -> Self {
        Self::new()
    }
}

pub async fn accept_tunnel_clients(listener: TcpListener, hub: Arc<TunnelHub>) {
    loop {
        let (stream, _) = match listener.accept().await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "tunnel acceptor accept failed");
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }
        };
        let hub = hub.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_inbound_client(stream, hub).await {
                tracing::debug!(error = %e, "tunnel client handshake failed");
            }
        });
    }
}

async fn handle_inbound_client(mut stream: TcpStream, hub: Arc<TunnelHub>) -> Result<()> {
    let line = tokio::time::timeout(Duration::from_secs(5), read_line(&mut stream))
        .await
        .context("handshake timed out")??;
    let (host_port, _internal) = parse_hello(&line).map_err(|e| anyhow::anyhow!(e))?;
    if !hub.is_live(host_port).await {
        let _ = stream.write_all(encode_err("unknown host_port").as_bytes()).await;
        anyhow::bail!("unknown host_port {host_port}");
    }
    stream
        .write_all(encode_ok().as_bytes())
        .await
        .context("writing handshake OK")?;
    hub.offer_client(host_port, stream).await;
    Ok(())
}

/// Bind `127.0.0.1:host_port` and pair each inbound TCP stream with a container client.
pub async fn start_host_forwarder(
    hub: Arc<TunnelHub>,
    mapping_id: String,
    host_port: u16,
) -> Result<()> {
    let listener = TcpListener::bind(("127.0.0.1", host_port))
        .await
        .with_context(|| format!("binding 127.0.0.1:{host_port}"))?;
    hub.open_port(host_port).await;
    let hub_accept = hub.clone();
    let handle = tokio::spawn(async move {
        loop {
            let (mut inbound, _) = match listener.accept().await {
                Ok(v) => v,
                Err(_) => break,
            };
            let hub = hub_accept.clone();
            tokio::spawn(async move {
                let Some(mut client) = hub.take_client(host_port, Duration::from_secs(20)).await
                else {
                    tracing::warn!(
                        host_port,
                        "no tunnel client ready for inbound connection"
                    );
                    return;
                };
                let _ = copy_bidirectional(&mut inbound, &mut client).await;
            });
        }
    });
    hub.track_job(&mapping_id, handle.abort_handle()).await;
    Ok(())
}

async fn connect_local(internal_port: u16) -> Result<TcpStream> {
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let mut delay = Duration::from_millis(50);
    loop {
        match TcpStream::connect(("127.0.0.1", internal_port)).await {
            Ok(s) => return Ok(s),
            Err(e) => {
                if std::time::Instant::now() >= deadline {
                    return Err(e).context(format!(
                        "connecting to 127.0.0.1:{internal_port} (local service)"
                    ));
                }
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(Duration::from_millis(800));
            }
        }
    }
}

/// One handshake + one bidirectional copy. The supervisor reconnects.
pub async fn tunnel_client_once(
    gateway: &str,
    host_port: u16,
    internal_port: u16,
) -> Result<()> {
    let mut remote = TcpStream::connect(gateway)
        .await
        .with_context(|| format!("connecting to tunnel {gateway}"))?;
    remote
        .write_all(encode_hello(host_port, internal_port).as_bytes())
        .await
        .context("sending handshake")?;
    let reply = tokio::time::timeout(Duration::from_secs(5), read_line(&mut remote))
        .await
        .context("handshake reply timed out")??;
    parse_reply(&reply).map_err(|e| anyhow::anyhow!("handshake: {e}"))?;
    let mut local = connect_local(internal_port).await?;
    let _ = copy_bidirectional(&mut remote, &mut local).await;
    Ok(())
}

pub async fn run_tunnel_client(
    gateway: &str,
    host_port: u16,
    internal_port: u16,
) -> Result<()> {
    loop {
        if let Err(e) = tunnel_client_once(gateway, host_port, internal_port).await {
            eprintln!("[nemesis8-entry] tunnel client: {e}");
            tokio::time::sleep(Duration::from_millis(400)).await;
        }
    }
}

pub fn run_tunnel_client_blocking(
    gateway: &str,
    host_port: u16,
    internal_port: u16,
) -> Result<()> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("tokio runtime for tunnel client")?;
    rt.block_on(run_tunnel_client(gateway, host_port, internal_port))
}

/// Bind the sibling tunnel acceptor. Occupied port → `PortOccupied`.
pub async fn bind_tunnel_acceptor(
    bind: &str,
    tunnel_port: u16,
) -> std::result::Result<TcpListener, TunnelDisableReason> {
    let addr = format!("{bind}:{tunnel_port}");
    match TcpListener::bind(&addr).await {
        Ok(l) => Ok(l),
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            Err(TunnelDisableReason::PortOccupied { port: tunnel_port })
        }
        Err(e) => Err(TunnelDisableReason::StartFailed {
            detail: format!("bind {addr}: {e}"),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::path::PathBuf;

    fn free_loopback_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    #[test]
    fn exact_port_allocates_when_free() {
        let port = free_loopback_port();
        let used = HashSet::new();
        assert_eq!(allocate_host_port(&used, Some(port), false), Ok(port));
    }

    #[test]
    fn exact_port_conflict_when_reserved() {
        let mut used = HashSet::new();
        used.insert(1455);
        assert_eq!(
            allocate_host_port(&used, Some(1455), false),
            Err(HostPortError::ExactBusy(1455))
        );
    }

    #[test]
    fn exact_port_conflict_when_accepting() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let used = HashSet::new();
        assert_eq!(
            allocate_host_port(&used, Some(port), false),
            Err(HostPortError::ExactBusy(port))
        );
    }

    #[test]
    fn exact_port_zero_is_busy() {
        let used = HashSet::new();
        assert_eq!(
            allocate_host_port(&used, Some(0), false),
            Err(HostPortError::ExactBusy(0))
        );
    }

    #[test]
    fn hello_roundtrip() {
        let line = encode_hello(1455, 1455);
        assert_eq!(parse_hello(&line).unwrap(), (1455, 1455));
        parse_reply(&encode_ok()).unwrap();
        assert!(parse_reply(&encode_err("nope")).unwrap_err().contains("nope"));
        assert!(parse_hello("N8TUNNEL/1").is_err());
        assert!(parse_hello("N8TUNNEL/1 1 2 extra").is_err());
    }

    #[test]
    fn disable_reason_distinguishes_occupied_from_bind_fail() {
        let occupied = TunnelDisableReason::PortOccupied { port: 9803 }.message();
        let failed = TunnelDisableReason::StartFailed {
            detail: "permission denied".into(),
        }
        .message();
        assert!(occupied.contains("occupied"), "{occupied}");
        assert!(occupied.contains("9803"), "{occupied}");
        assert!(!failed.contains("occupied"), "{failed}");
        assert!(failed.contains("failed to bind"), "{failed}");
        assert!(!occupied.to_lowercase().contains("chisel"), "{occupied}");
        assert!(!failed.to_lowercase().contains("chisel"), "{failed}");
    }

    #[test]
    fn declared_callback_ports_include_provider_toml_ports() {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("providers");
        unsafe { std::env::set_var("NEMESIS8_PROVIDERS_DIR", &dir) };
        let ports = declared_callback_ports();
        assert!(
            ports.contains(&1455),
            "expected 1455 from fx/codex TOML, got {ports:?}"
        );
        assert!(
            ports.contains(&54545),
            "expected 54545 from omp TOML, got {ports:?}"
        );
    }

    #[tokio::test]
    async fn forwards_bytes_host_to_internal() {
        let internal = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let internal_port = internal.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut s, _) = internal.accept().await.unwrap();
            let mut buf = [0u8; 32];
            let n = s.read(&mut buf).await.unwrap();
            s.write_all(&buf[..n]).await.unwrap();
        });

        let hub = Arc::new(TunnelHub::new());
        let acceptor = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let tun_addr = acceptor.local_addr().unwrap();
        tokio::spawn(accept_tunnel_clients(acceptor, hub.clone()));

        let host_bind = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let host_port = host_bind.local_addr().unwrap().port();
        drop(host_bind);

        start_host_forwarder(hub.clone(), "map-1".into(), host_port)
            .await
            .unwrap();

        let gw = format!("127.0.0.1:{}", tun_addr.port());
        tokio::spawn(async move {
            let _ = tunnel_client_once(&gw, host_port, internal_port).await;
        });

        let mut inbound = None;
        for _ in 0..50 {
            if let Ok(s) = TcpStream::connect(("127.0.0.1", host_port)).await {
                inbound = Some(s);
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let mut inbound = inbound.expect("host listener never came up");
        inbound.write_all(b"ping").await.unwrap();
        let mut buf = [0u8; 8];
        let n = tokio::time::timeout(Duration::from_secs(2), inbound.read(&mut buf))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&buf[..n], b"ping");
    }
}
