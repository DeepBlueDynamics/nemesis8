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
use std::io::Write;
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
    /// removed, accessed. `delta_bytes` carries (new_size - old_size) when
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
}

/// Where events get written. Anything that can `write_event` qualifies; the
/// default impl is a line-delimited JSON file on the persistent mount.
pub trait EventSink: Send {
    fn write_event(&mut self, event: &MonitorEvent) -> Result<()>;
}

pub struct JsonlSink {
    path: PathBuf,
}

impl JsonlSink {
    pub fn new(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("creating monitor events dir {}", parent.display())
            })?;
        }
        Ok(Self { path })
    }
}

impl EventSink for JsonlSink {
    fn write_event(&mut self, event: &MonitorEvent) -> Result<()> {
        let line = serde_json::to_string(event)?;
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .with_context(|| format!("opening {}", self.path.display()))?;
        writeln!(f, "{line}")?;
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

    let detail = match event.kind {
        EventKind::Create(_) => "created",
        EventKind::Modify(_) => "modified",
        EventKind::Remove(_) => "removed",
        EventKind::Access(_) => "accessed",
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
