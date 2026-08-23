//! Runtime port-exposure registry.
//!
//! This module intentionally stops at the control-plane layer: allocate a
//! host-loopback port, track mappings, and expose serializable API types. The
//! data plane is deliberately left to the v1 transport chosen in
//! `docs/plans/serve-port-tunnel.md` (currently chisel on a sibling tunnel
//! port), not an in-gateway native WebSocket mux.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

const PORT_RANGE_START: u16 = 18000;
const PORT_RANGE_END: u16 = 18999;
// OFFSET 2, NOT 1: gateway+1 (9802) is the trainer API's port. When the port
// family moved to 9801/9802 (2026-07), the +1 sibling landed exactly on the
// trainer — the chisel server could never bind, but the readiness probe saw
// the TRAINER'S loopback socket and declared the tunnel plane healthy, so
// every expose_port mapping sat "pending" forever while reporting success.
// 9803 is the chisel reverse-server's reserved slot in the port family.
pub const DEFAULT_TUNNEL_PORT_OFFSET: u16 = 2;
pub const CHISEL_VERSION: &str = "1.11.5";

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
    sidecar_reserved: bool,
) -> Result<u16, HostPortError> {
    if let Some(requested) = requested {
        if requested == 0 || used.contains(&requested) || port_accepts(requested) {
            return Err(HostPortError::ExactBusy(requested));
        }
        return Ok(requested);
    }
    let allocated = if sidecar_reserved {
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

pub struct ChiselServer {
    pub reverse_bind_host: &'static str,
    pub ports_reserved_by_sidecar: bool,
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

/// Registry of allocated port mappings. Transport-specific live-tunnel state
/// belongs to the eventual chisel/native data-plane supervisor, not here.
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

pub fn find_chisel_binary() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("NEMESIS8_CHISEL") {
        let p = PathBuf::from(path);
        if p.is_file() {
            return Some(p);
        }
    }

    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            for name in chisel_names() {
                let p = dir.join(name);
                if p.is_file() {
                    return Some(p);
                }
            }
        }
    }

    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths).find_map(|dir| {
            chisel_names()
                .into_iter()
                .map(|name| dir.join(name))
                .find(|p| p.is_file())
        })
    })
}

fn chisel_names() -> Vec<&'static str> {
    #[cfg(windows)]
    {
        vec!["chisel.exe", "chisel"]
    }
    #[cfg(not(windows))]
    {
        vec!["chisel"]
    }
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

pub fn ensure_chisel_server(tunnel_port: u16, runtime: &str) -> Result<ChiselServer> {
    let Some(chisel) = find_chisel_binary() else {
        ensure_chisel_server_container(tunnel_port, runtime)?;
        return Ok(ChiselServer {
            reverse_bind_host: "0.0.0.0",
            ports_reserved_by_sidecar: true,
        });
    };
    if port_accepts(tunnel_port) {
        return Ok(ChiselServer {
            reverse_bind_host: "127.0.0.1",
            ports_reserved_by_sidecar: false,
        });
    }
    let mut cmd = Command::new(&chisel);
    cmd.args([
        "server",
        "--reverse",
        "--host",
        "127.0.0.1",
        "--port",
        &tunnel_port.to_string(),
    ])
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0000_0008 | 0x0000_0200);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    cmd.spawn()
        .with_context(|| format!("starting chisel reverse server on 127.0.0.1:{tunnel_port}"))?;
    Ok(ChiselServer {
        reverse_bind_host: "127.0.0.1",
        ports_reserved_by_sidecar: false,
    })
}

fn ensure_chisel_server_container(tunnel_port: u16, runtime: &str) -> Result<()> {
    let name = format!("nemesis8-chisel-{tunnel_port}");
    let _ = Command::new(runtime)
        .args(["rm", "-f", &name])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let port = tunnel_port.to_string();
    let publish = format!("127.0.0.1:{port}:{port}");
    // Publish the whole reverse-tunnel range so any allocated mapping is
    // reachable from the host. A single Docker range publish — bollard's
    // port-binding map can't express this compactly, which is why the tunnel
    // layer launches chisel via the runtime CLI rather than `ensure_service`.
    let publish_range = format!(
        "127.0.0.1:{PORT_RANGE_START}-{PORT_RANGE_END}:{PORT_RANGE_START}-{PORT_RANGE_END}"
    );

    // Image + command are declared in services/chisel.toml (declarative source);
    // we only own the dynamic port + range publishes here.
    let (image, command) = chisel_image_and_command(&port);

    let mut args: Vec<String> = vec![
        "run".into(),
        "-d".into(),
        "--rm".into(),
        "--name".into(),
        name.clone(),
        "-p".into(),
        publish,
        "-p".into(),
        publish_range,
        image,
    ];
    args.extend(command);
    let status = Command::new(runtime)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .with_context(|| format!("starting chisel sidecar container on 127.0.0.1:{port}"))?;
    if !status.success() {
        anyhow::bail!("chisel sidecar container exited with {status}");
    }
    Ok(())
}

/// Resolve chisel's image + command from `services/chisel.toml` (with `{port}`
/// substituted), falling back to built-in defaults if the spec is absent or
/// malformed. Keeps the chisel image/version/args declarative without forcing
/// the dynamic-port + range-publish mechanics through `ensure_service`.
fn chisel_image_and_command(port: &str) -> (String, Vec<String>) {
    let reg = crate::service_registry::ServiceRegistry::load();
    if let Some(def) = reg.get("chisel") {
        if let Some(img) = &def.service.image {
            let cmd: Vec<String> = def
                .service
                .command
                .iter()
                .map(|a| a.replace("{port}", port))
                .collect();
            if !cmd.is_empty() {
                return (img.clone(), cmd);
            }
        }
    }
    (
        format!("jpillora/chisel:{CHISEL_VERSION}"),
        vec![
            "server".into(),
            "--reverse".into(),
            "--host".into(),
            "0.0.0.0".into(),
            "--port".into(),
            port.to_string(),
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

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
}
