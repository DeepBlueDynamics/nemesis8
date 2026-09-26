//! Nemesis internal agent — a always-on telemetry/monitoring layer inside the
//! container. Runs in parallel with the AI provider (codex / gemini / agy /
//! claude / etc.) and emits a stream of structured events describing what the
//! agent's process tree is doing: files opened or changed, network destinations
//! contacted, status transitions.
//!
//! Events are written to `/opt/nemesis8/.monitor/events.jsonl` on the
//! persistent mount. The gateway exposes them via HTTP for downstream
//! dashboards. The persistent location means events survive container restarts
//! and can be inspected from the host without needing the container running.
//!
//! Stub status (v0.7.18):
//!   - Event schema and JSONL sink: implemented.
//!   - Filesystem watcher (notify-based): implemented for /workspace.
//!   - Network watcher: not implemented (placeholder type only).
//!   - Diff capture for file modifications: not implemented (size delta only).
//!   - Process tree watcher: not implemented.

use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub const EVENTS_DIR: &str = "/opt/nemesis8/.monitor";
pub const EVENTS_FILE: &str = "/opt/nemesis8/.monitor/events.jsonl";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MonitorEvent {
    /// Monitor heartbeat — emitted periodically so consumers can tell the
    /// agent is alive even when no other activity is occurring.
    Heartbeat { ts: u64, pid: u32 },

    /// A status transition from the monitor itself.
    Status { ts: u64, status: String, msg: String },

    /// Filesystem activity. `kind_detail` is one of: created, modified,
    /// removed (inotify Access events are dropped at the source — a read is
    /// not a change). `delta_bytes` carries (new_size - old_size) when
    /// known; zero otherwise.
    Fs {
        ts: u64,
        path: String,
        kind_detail: String,
        size_bytes: u64,
        delta_bytes: i64,
    },

    /// Outbound network destination. Stub — not currently emitted.
    Net {
        ts: u64,
        protocol: String,
        dest: String,
        port: u16,
        bytes_sent: u64,
        bytes_recv: u64,
    },

    /// Process tree change. Stub — not currently emitted.
    Proc { ts: u64, pid: u32, cmd: String, action: String },

    /// Periodic host/container metrics snapshot (LOGPANE EPIC 1).
    Metric {
        ts: u64,
        cpu_pct: f64,
        mem_used_kb: u64,
        mem_total_kb: u64,
        load1: f64,
        net_rx_bps: u64,
        net_tx_bps: u64,
    },

    /// A newly-appended line from a watched `*.log` under the workspace,
    /// Splunk-style (LOGPANE EPIC 1).
    LogLine { ts: u64, path: String, line: String },

    /// A file edit made through the nuts-files MCP server (`nuts_edit`,
    /// `nuts_replace`, `nuts_write`) — the one place that knows which LINES
    /// changed. Written by nuts-files into the same events file (it is a
    /// separate crate, so it emits this shape as raw JSON); the gateway pushes
    /// it to Hyperia as `Edit`. Lines are 1-based; `regions` is capped at 20;
    /// `substitutions` is nonzero only for `nuts_replace`.
    Edit {
        ts: u64,
        tool: String,
        path: String,
        lines_added: u64,
        lines_removed: u64,
        substitutions: u64,
        regions: Vec<EditRegion>,
        bytes_before: u64,
        bytes_after: u64,
    },
}

/// A contiguous line range touched by an edit (1-based, inclusive).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EditRegion {
    pub start_line: u64,
    pub end_line: u64,
}

/// Where events get written. Anything that can `write_event` qualifies; the
/// default impl is a line-delimited JSON file on the persistent mount.
pub trait EventSink: Send {
    fn write_event(&mut self, event: &MonitorEvent) -> Result<()>;
}

pub struct JsonlSink {
    path: PathBuf,
    /// Producing container's identity (NEMESIS8_AGENT_ID), stamped onto every
    /// event so a shared events.jsonl can be demultiplexed per agent (Phase A).
    agent_id: Option<String>,
    /// Rotate the file once it exceeds this many bytes; one `.1` backup is kept.
    max_bytes: u64,
    /// Running size of the current file, so we don't stat on every write.
    size: u64,
}

impl JsonlSink {
    pub fn new(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("creating monitor events dir {}", parent.display())
            })?;
        }
        let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        let agent_id = std::env::var("NEMESIS8_AGENT_ID").ok().filter(|s| !s.is_empty());
        Ok(Self {
            path,
            agent_id,
            max_bytes: 32 * 1024 * 1024,
            size,
        })
    }
}

impl EventSink for JsonlSink {
    fn write_event(&mut self, event: &MonitorEvent) -> Result<()> {
        // Phase A — tag with the producing agent's identity so the host can tell
        // agents apart in the shared stream.
        let line = match &self.agent_id {
            Some(id) => {
                let mut v = serde_json::to_value(event)?;
                if let Some(obj) = v.as_object_mut() {
                    obj.insert("agent_id".into(), serde_json::Value::String(id.clone()));
                }
                serde_json::to_string(&v)?
            }
            None => serde_json::to_string(event)?,
        };
        // Phase B — rotate so the live file stays bounded (a chatty fs watcher
        // otherwise grows it without limit). Rename to .1 and start fresh.
        if self.size + line.len() as u64 + 1 > self.max_bytes {
            let _ = std::fs::rename(&self.path, self.path.with_extension("jsonl.1"));
            self.size = 0;
        }
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .with_context(|| format!("opening {}", self.path.display()))?;
        writeln!(f, "{line}")?;
        self.size += line.len() as u64 + 1;
        Ok(())
    }
}

// Plain-HTTP (no TLS) POST helpers for the entry's gateway registration and
// pulse. Best-effort callers swallow the error.
// ── Pooled keep-alive HTTP client ───────────────────────────────────────────
//
// The monitor pushes EVERY event through here — fs-watch events on the
// workspace, a pulse tick every 2 s, metrics every 5 s, one event per newly
// appended agent log line every 3 s, heartbeats — and so do pulse and entry.
// It used to open a fresh TCP connection per POST, send `Connection: close`,
// and hang up without reading the reply. A chatty agent (grok, codex) turned
// that into ~25 new connections/s, and the host ran out of ephemeral ports:
// 15,475 sockets in TIME_WAIT, 14,383 of them to 127.0.0.1:9801 (2026-09-24).
//
// Now one connection per host:port is kept and reused: send with keep-alive,
// read the whole reply so the framing stays in sync, park the stream. If a
// parked stream turns out dead (the server closed it while idle), reconnect
// once and retry. Callers are unchanged and still best-effort.

fn conn_pool() -> &'static std::sync::Mutex<std::collections::HashMap<String, std::net::TcpStream>> {
    static POOL: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<String, std::net::TcpStream>>,
    > = std::sync::OnceLock::new();
    POOL.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

fn pool_take(hostport: &str) -> Option<std::net::TcpStream> {
    conn_pool().lock().ok().and_then(|mut p| p.remove(hostport))
}

fn pool_put(hostport: &str, stream: std::net::TcpStream) {
    if let Ok(mut p) = conn_pool().lock() {
        p.insert(hostport.to_string(), stream);
    }
}

/// POST JSON over a pooled keep-alive connection; returns (status, body).
fn http_post_json_pooled(
    url: &str,
    body: &str,
    token: Option<&str>,
) -> std::io::Result<(u16, String)> {
    let (hostport, path) = split_http_url(url)?;
    let mut retried = false;
    loop {
        let (mut stream, reused) = match pool_take(&hostport) {
            Some(s) => (s, true),
            None => {
                let s = std::net::TcpStream::connect(&hostport)?;
                let _ = s.set_nodelay(true);
                (s, false)
            }
        };
        let result = write_post_request(&mut stream, &hostport, &path, body, token)
            .and_then(|_| read_http_response_ex(&mut stream));
        match result {
            Ok((status, resp_body, reusable)) => {
                if reusable {
                    pool_put(&hostport, stream);
                }
                return Ok((status, resp_body));
            }
            // A parked stream the server closed while idle: reconnect, retry once.
            Err(_) if reused && !retried => {
                retried = true;
                continue;
            }
            Err(e) => return Err(e),
        }
    }
}

/// POST JSON, best-effort; the reply is read (and discarded) so the pooled
/// connection can be reused.
pub fn http_post_json(url: &str, body: &str, token: Option<&str>) -> std::io::Result<()> {
    http_post_json_pooled(url, body, token).map(|_| ())
}

/// POST JSON and wait through response headers (and body, when advertised).
/// Used by entry for gateway register and OAuth `/expose`, which must not
/// report success on 4xx/5xx.
pub fn http_post_json_response(
    url: &str,
    body: &str,
    token: Option<&str>,
) -> std::io::Result<(u16, String)> {
    http_post_json_pooled(url, body, token)
}

/// POST JSON and treat only 2xx as success. Non-2xx includes the response body
/// when present (gateway `{"error":...}` for port conflicts, etc.).
pub fn http_post_json_ok(url: &str, body: &str, token: Option<&str>) -> std::io::Result<()> {
    let (status, resp_body) = http_post_json_response(url, body, token)?;
    if (200..300).contains(&status) {
        Ok(())
    } else {
        let detail = resp_body.trim();
        if detail.is_empty() {
            Err(std::io::Error::other(format!("HTTP {status}")))
        } else {
            Err(std::io::Error::other(format!("HTTP {status}: {detail}")))
        }
    }
}

fn split_http_url(url: &str) -> std::io::Result<(String, String)> {
    let rest = url.strip_prefix("http://").ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "only http:// supported")
    })?;
    Ok(match rest.split_once('/') {
        Some((h, p)) => (h.to_string(), format!("/{p}")),
        None => (rest.to_string(), "/".to_string()),
    })
}

/// Write one keep-alive POST. The reply MUST then be read (see
/// `read_http_response_ex`) before the stream carries another request.
fn write_post_request(
    stream: &mut std::net::TcpStream,
    hostport: &str,
    path: &str,
    body: &str,
    token: Option<&str>,
) -> std::io::Result<()> {
    let auth = token
        .map(|t| format!("Authorization: Bearer {t}\r\n"))
        .unwrap_or_default();
    let req = format!(
        "POST {path} HTTP/1.1\r\nHost: {hostport}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{auth}Connection: keep-alive\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(req.as_bytes())?;
    stream.flush()
}

/// Read one HTTP response: (status, body, reusable). `reusable` is true only
/// when the reply was fully delimited — a Content-Length that arrived in full
/// and no `Connection: close` — so the stream can carry the next request. A
/// reply read to EOF/timeout leaves the stream out of sync: don't reuse it.
fn read_http_response_ex(
    stream: &mut std::net::TcpStream,
) -> std::io::Result<(u16, String, bool)> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut buf = [0u8; 1024];
    let mut collected = Vec::new();
    let header_end = loop {
        match stream.read(&mut buf) {
            Ok(0) => break None,
            Ok(n) => {
                collected.extend_from_slice(&buf[..n]);
                if let Some(pos) = find_header_end(&collected) {
                    break Some(pos);
                }
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                break None;
            }
            Err(e) => return Err(e),
        }
    };
    let Some(header_end) = header_end else {
        return Ok((parse_http_status(&collected)?, String::new(), false));
    };
    let headers = collected[..header_end].to_vec();
    let mut body = collected[header_end..].to_vec();
    let status = parse_http_status(&headers)?;
    let wants_close = header_says_close(&headers);
    // Bodiless statuses (RFC 9110 §6.4.1): 1xx, 204 and 304 never carry a body,
    // and hyper sends them WITHOUT Content-Length. Falling through to the
    // read-to-EOF branch blocked until the 5 s read timeout — which is exactly
    // what the entry's exit-time deregister (a 204) did on every container
    // shutdown: the agent had quit, the container lingered, then died.
    if (100..200).contains(&status) || status == 204 || status == 304 {
        return Ok((status, String::new(), !wants_close));
    }
    if let Some(len) = parse_content_length(&headers) {
        let mut complete = body.len() >= len;
        while body.len() < len {
            match stream.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    body.extend_from_slice(&buf[..n]);
                    complete = body.len() >= len;
                }
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut =>
                {
                    break;
                }
                Err(e) => return Err(e),
            }
        }
        body.truncate(len);
        let text = String::from_utf8_lossy(&body).into_owned();
        Ok((status, text, complete && !wants_close))
    } else {
        // No Content-Length: read to EOF/timeout (legacy servers). Not reusable.
        loop {
            match stream.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => body.extend_from_slice(&buf[..n]),
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut =>
                {
                    break;
                }
                Err(e) => return Err(e),
            }
        }
        Ok((status, String::from_utf8_lossy(&body).into_owned(), false))
    }
}

fn header_says_close(headers: &[u8]) -> bool {
    String::from_utf8_lossy(headers).lines().any(|line| {
        line.split_once(':').is_some_and(|(name, value)| {
            name.eq_ignore_ascii_case("connection") && value.trim().eq_ignore_ascii_case("close")
        })
    })
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| i + 4)
        .or_else(|| buf.windows(2).position(|w| w == b"\n\n").map(|i| i + 2))
}

fn parse_content_length(headers: &[u8]) -> Option<usize> {
    let text = String::from_utf8_lossy(headers);
    for line in text.lines() {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.eq_ignore_ascii_case("content-length") {
            return value.trim().parse().ok();
        }
    }
    None
}

fn parse_http_status(resp: &[u8]) -> std::io::Result<u16> {
    let text = String::from_utf8_lossy(resp);
    let line = text.lines().next().unwrap_or("");
    let code = line.split_whitespace().nth(1).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("no HTTP status line in {text:?}"),
        )
    })?;
    code.parse::<u16>().map_err(|e| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, format!("bad HTTP status: {e}"))
    })
}

/// Fan-out sink: write each event to every contained sink.
pub struct TeeSink {
    sinks: Vec<Box<dyn EventSink>>,
}

impl TeeSink {
    pub fn new(sinks: Vec<Box<dyn EventSink>>) -> Self {
        Self { sinks }
    }
}

impl EventSink for TeeSink {
    fn write_event(&mut self, event: &MonitorEvent) -> Result<()> {
        for s in self.sinks.iter_mut() {
            let _ = s.write_event(event);
        }
        Ok(())
    }
}

pub fn now_ts() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Run the monitor loop forever. Watches `watch_dirs` for filesystem activity
/// and emits a heartbeat every `heartbeat_secs`. Blocks; intended to be called
/// from the monitor binary's main().
pub fn run_monitor(
    watch_dirs: &[&Path],
    heartbeat_secs: u64,
    sink: &mut dyn EventSink,
) -> Result<()> {
    use notify::{Event, RecursiveMode, Watcher};

    let pid = std::process::id();
    sink.write_event(&MonitorEvent::Status {
        ts: now_ts(),
        status: "started".to_string(),
        msg: format!("nemesis8-monitor pid={pid}, watching {} dir(s)", watch_dirs.len()),
    })?;

    let (tx, rx) = mpsc::channel::<notify::Result<Event>>();

    let mut watcher = notify::recommended_watcher(move |res| {
        let _ = tx.send(res);
    })?;

    for dir in watch_dirs {
        if dir.exists() {
            if let Err(e) = watcher.watch(dir, RecursiveMode::Recursive) {
                sink.write_event(&MonitorEvent::Status {
                    ts: now_ts(),
                    status: "watch_error".to_string(),
                    msg: format!("could not watch {}: {e}", dir.display()),
                })?;
            }
        }
    }

    let mut last_heartbeat = std::time::Instant::now();
    let mut last_pulse_tick = std::time::Instant::now();
    let mut pulse_emitter = crate::pulse::PulseEmitter::new();
    let mut activity_state = crate::activity::ActivityState::default();

    // LOGPANE EPIC 1 collectors: periodic metrics + Splunk-style log tail. Both
    // feed the same sink fan-out as everything else.
    let mut metrics_collector = crate::collectors::MetricsCollector::new();
    let mut log_tailer = crate::collectors::LogTailer::new("/workspace");
    let mut last_metrics = std::time::Instant::now();
    let mut last_tail = std::time::Instant::now();

    loop {
        match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(Ok(event)) => {
                emit_fs_event(sink, &event);
            }
            Ok(Err(e)) => {
                let _ = sink.write_event(&MonitorEvent::Status {
                    ts: now_ts(),
                    status: "watch_error".to_string(),
                    msg: format!("notify error: {e}"),
                });
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                let _ = sink.write_event(&MonitorEvent::Status {
                    ts: now_ts(),
                    status: "stopped".to_string(),
                    msg: "watcher channel disconnected".to_string(),
                });
                break;
            }
        }

        let now_instant = std::time::Instant::now();
        if now_instant.duration_since(last_pulse_tick) >= Duration::from_secs(2) {
            let cpu_min = std::env::var("PULSE_CPU_MIN")
                .ok()
                .and_then(|s| s.parse::<f64>().ok())
                .unwrap_or(1.0);
            let net_min = std::env::var("PULSE_NET_MIN")
                .ok()
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(128);
            let io_min = std::env::var("PULSE_IO_MIN")
                .ok()
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(128);

            let sample = crate::activity::sample(&mut activity_state);
            let is_busy = sample.busy(cpu_min, net_min, io_min);
            pulse_emitter.tick(is_busy, sink);
            last_pulse_tick = now_instant;
        }

        // Metrics snapshot every 5s.
        if now_instant.duration_since(last_metrics) >= Duration::from_secs(5) {
            let m = metrics_collector.sample();
            let _ = sink.write_event(&MonitorEvent::Metric {
                ts: now_ts(),
                cpu_pct: m.cpu_pct,
                mem_used_kb: m.mem_used_kb,
                mem_total_kb: m.mem_total_kb,
                load1: m.load1,
                net_rx_bps: m.net_rx_bps,
                net_tx_bps: m.net_tx_bps,
            });
            last_metrics = now_instant;
        }

        // Log tail every 3s: emit each newly-appended *.log line.
        if now_instant.duration_since(last_tail) >= Duration::from_secs(3) {
            for (path, line) in log_tailer.poll() {
                let _ = sink.write_event(&MonitorEvent::LogLine {
                    ts: now_ts(),
                    path,
                    line,
                });
            }
            last_tail = now_instant;
        }

        if last_heartbeat.elapsed() >= Duration::from_secs(heartbeat_secs) {
            let _ = sink.write_event(&MonitorEvent::Heartbeat {
                ts: now_ts(),
                pid,
            });
            last_heartbeat = std::time::Instant::now();
        }
    }

    Ok(())
}

fn emit_fs_event(sink: &mut dyn EventSink, event: &notify::Event) {
    use notify::EventKind;

    // Access events (inotify IN_ACCESS: a file read or a directory listing)
    // carry no state change. They were 93 % of all telemetry on one host —
    // every `ls` by any tool, duplicated by every container watching the same
    // workspace — so they are dropped at the source.
    if matches!(event.kind, EventKind::Access(_)) {
        return;
    }
    let detail = match event.kind {
        EventKind::Create(_) => "created",
        EventKind::Modify(_) => "modified",
        EventKind::Remove(_) => "removed",
        _ => "other",
    };

    for path in &event.paths {
        let size_bytes = path
            .metadata()
            .map(|m| m.len())
            .unwrap_or(0);
        let _ = sink.write_event(&MonitorEvent::Fs {
            ts: now_ts(),
            path: path.to_string_lossy().to_string(),
            kind_detail: detail.to_string(),
            size_bytes,
            delta_bytes: 0,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jsonl_sink_rotates_and_bounds_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let mut sink = JsonlSink::new(&path).unwrap();
        sink.max_bytes = 256; // force frequent rotation
        for ts in 0..50 {
            sink.write_event(&MonitorEvent::Heartbeat { ts, pid: 1 }).unwrap();
        }
        // a rotated backup exists and the live file stays bounded
        assert!(path.with_extension("jsonl.1").exists());
        assert!(std::fs::metadata(&path).unwrap().len() <= 256 + 128);
    }

    fn spawn_status_server(status: u16) -> (String, std::thread::JoinHandle<()>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let _ = s.read(&mut buf);
            let resp = format!(
                "HTTP/1.1 {status} X\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            let _ = s.write_all(resp.as_bytes());
        });
        (format!("http://127.0.0.1:{}", addr.port()), handle)
    }

    #[test]
    fn pooled_posts_reuse_one_connection() {
        // Two POSTs to the same host:port must ride ONE TCP connection. The old
        // connect + `Connection: close` per request, dropped without reading the
        // reply, exhausted the host's ephemeral ports (15k TIME_WAIT to :9801).
        // This server accepts exactly once and answers two requests on it.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = std::sync::mpsc::channel::<usize>();
        std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
            let mut served = 0usize;
            let mut buf = vec![0u8; 4096];
            let mut acc: Vec<u8> = Vec::new();
            while served < 2 {
                let n = match s.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                acc.extend_from_slice(&buf[..n]);
                // One request = headers + Content-Length body.
                while let Some(pos) = find_header_end(&acc) {
                    let len = parse_content_length(&acc[..pos]).unwrap_or(0);
                    if acc.len() < pos + len {
                        break;
                    }
                    acc.drain(..pos + len);
                    s.write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n")
                        .unwrap();
                    served += 1;
                }
            }
            let _ = tx.send(served);
        });
        let url = format!("http://{addr}/agents/x/events");
        http_post_json(&url, r#"{"a":1}"#, None).unwrap();
        http_post_json(&url, r#"{"b":2}"#, None).unwrap();
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            2,
            "both requests must be served on the single accepted connection"
        );
    }

    #[test]
    fn bodiless_204_without_content_length_returns_at_once_and_keeps_the_connection() {
        // hyper answers `-> StatusCode::NO_CONTENT` handlers (the gateway's
        // deregister) with no Content-Length at all. The reader used to treat
        // that as "read to EOF" and sat on the open keep-alive socket until its
        // 5 s timeout — every container shutdown paid it. Server: accepts once,
        // answers two requests with a bare 204, keeps the socket open.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = std::sync::mpsc::channel::<usize>();
        std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
            let mut served = 0usize;
            let mut buf = vec![0u8; 4096];
            let mut acc: Vec<u8> = Vec::new();
            while served < 2 {
                let n = match s.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                acc.extend_from_slice(&buf[..n]);
                while let Some(pos) = find_header_end(&acc) {
                    let len = parse_content_length(&acc[..pos]).unwrap_or(0);
                    if acc.len() < pos + len {
                        break;
                    }
                    acc.drain(..pos + len);
                    s.write_all(b"HTTP/1.1 204 No Content\r\ndate: x\r\n\r\n").unwrap();
                    served += 1;
                }
            }
            let _ = tx.send(served);
            // keep the socket open a little so a "read to EOF" reader would hang
            std::thread::sleep(Duration::from_millis(1500));
        });
        let url = format!("http://{addr}/agents/x/deregister");
        let t0 = std::time::Instant::now();
        let (status, body) = http_post_json_response(&url, "{}", None).unwrap();
        assert_eq!((status, body.as_str()), (204, ""));
        assert!(
            t0.elapsed() < Duration::from_millis(1000),
            "a 204 must return immediately, not after the read timeout ({:?})",
            t0.elapsed()
        );
        http_post_json(&url, "{}", None).unwrap();
        assert_eq!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), 2, "second request rode the same connection");
    }

    #[test]
    fn http_post_json_ok_accepts_2xx() {
        let (url, h) = spawn_status_server(204);
        http_post_json_ok(&format!("{url}/agents/x/register"), "{}", None).unwrap();
        h.join().unwrap();
    }

    #[test]
    fn http_post_json_ok_reports_non_2xx() {
        let (url, h) = spawn_status_server(503);
        let err = http_post_json_ok(&format!("{url}/expose"), "{}", None).unwrap_err();
        assert!(
            err.to_string().contains("HTTP 503"),
            "expected HTTP 503 in {err}"
        );
        h.join().unwrap();
    }

    #[test]
    fn http_post_json_ok_includes_error_body() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let _ = s.read(&mut buf);
            let payload = r#"{"error":"host callback port 1455 is already in use"}"#;
            let resp = format!(
                "HTTP/1.1 503 Service Unavailable\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                payload.len()
            );
            let _ = s.write_all(resp.as_bytes());
        });
        let url = format!("http://127.0.0.1:{}/expose", addr.port());
        let err = http_post_json_ok(&url, "{}", None).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("HTTP 503"), "expected HTTP 503 in {msg}");
        assert!(
            msg.contains("host callback port 1455 is already in use"),
            "expected conflict body in {msg}"
        );
        handle.join().unwrap();
    }

    #[test]
    fn register_completes_before_expose() {
        use std::sync::mpsc;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = mpsc::channel::<String>();
        std::thread::spawn(move || {
            for _ in 0..2 {
                let (mut s, _) = listener.accept().unwrap();
                let mut buf = [0u8; 4096];
                let n = s.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                let path = req.lines().next().unwrap_or("").to_string();
                tx.send(path).ok();
                std::thread::sleep(Duration::from_millis(40));
                let _ = s.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                );
            }
        });
        let base = format!("http://127.0.0.1:{}", addr.port());
        http_post_json_ok(&format!("{base}/agents/x/register"), "{}", None).unwrap();
        http_post_json_ok(&format!("{base}/expose"), "{}", None).unwrap();
        let first = rx.recv().unwrap();
        let second = rx.recv().unwrap();
        assert!(
            first.contains("/agents/x/register"),
            "first request should be register, got {first}"
        );
        assert!(
            second.contains("/expose"),
            "second request should be expose, got {second}"
        );
    }
}
