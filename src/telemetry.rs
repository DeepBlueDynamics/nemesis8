use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use crate::event_index::EventIndex;
use serde::{Serialize, Deserialize};

#[derive(Clone)]
pub struct TelemetryState {
    pub index: Arc<Mutex<EventIndex>>,
    pub events_path: PathBuf,
    pub sibling_path: PathBuf,
    pub events_mtime: Arc<Mutex<Option<std::time::SystemTime>>>,
    pub events_size: Arc<Mutex<u64>>,
    pub sibling_mtime: Arc<Mutex<Option<std::time::SystemTime>>>,
    pub sibling_size: Arc<Mutex<u64>>,
    pub cap: usize,
}

impl TelemetryState {
    pub fn new(cap: usize) -> Self {
        let home = crate::paths::data_home();
        let monitor_dir = home.join(".monitor");
        let events_path = monitor_dir.join("events.jsonl");
        let sibling_path = monitor_dir.join("events.jsonl.1");
        Self {
            index: Arc::new(Mutex::new(EventIndex::new(cap))),
            events_path,
            sibling_path,
            events_mtime: Arc::new(Mutex::new(None)),
            events_size: Arc::new(Mutex::new(0)),
            sibling_mtime: Arc::new(Mutex::new(None)),
            sibling_size: Arc::new(Mutex::new(0)),
            cap,
        }
    }

    pub fn refresh(&self) {
        let mut changed = false;

        let (e_mtime, e_size) = std::fs::metadata(&self.events_path)
            .map(|meta| (Some(meta.modified().unwrap_or(std::time::UNIX_EPOCH)), meta.len()))
            .unwrap_or((None, 0));

        let (s_mtime, s_size) = std::fs::metadata(&self.sibling_path)
            .map(|meta| (Some(meta.modified().unwrap_or(std::time::UNIX_EPOCH)), meta.len()))
            .unwrap_or((None, 0));

        let mut events_mtime_guard = self.events_mtime.lock().unwrap_or_else(|p| p.into_inner());
        let mut events_size_guard = self.events_size.lock().unwrap_or_else(|p| p.into_inner());
        let mut sibling_mtime_guard = self.sibling_mtime.lock().unwrap_or_else(|p| p.into_inner());
        let mut sibling_size_guard = self.sibling_size.lock().unwrap_or_else(|p| p.into_inner());

        if e_mtime != *events_mtime_guard || e_size != *events_size_guard {
            changed = true;
        }
        if s_mtime != *sibling_mtime_guard || s_size != *sibling_size_guard {
            changed = true;
        }

        if changed {
            let mut new_index = EventIndex::new(self.cap);
            // Load sibling (older events) first
            if s_mtime.is_some() {
                let _ = read_tail_into_index(&mut new_index, &self.sibling_path, self.cap);
            }
            // Load main events file
            if e_mtime.is_some() {
                let _ = read_tail_into_index(&mut new_index, &self.events_path, self.cap);
            }
            let mut index_guard = self.index.lock().unwrap_or_else(|p| p.into_inner());
            *index_guard = new_index;
            *events_mtime_guard = e_mtime;
            *events_size_guard = e_size;
            *sibling_mtime_guard = s_mtime;
            *sibling_size_guard = s_size;
        }
    }
}

fn read_tail_into_index(index: &mut EventIndex, path: &Path, cap: usize) -> std::io::Result<()> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path)?;
    let size = f.metadata()?.len();
    let window = (cap as u64).saturating_mul(256).min(32 * 1024 * 1024);
    let start = size.saturating_sub(window);
    f.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    f.read_to_end(&mut bytes)?;
    let text = String::from_utf8_lossy(&bytes);
    let body: &str = if start > 0 {
        text.find('\n').map(|i| &text[i + 1..]).unwrap_or("")
    } else {
        &text
    };
    for line in body.lines() {
        let line = line.trim();
        if !line.is_empty() {
            index.ingest_line(line);
        }
    }
    Ok(())
}

/// One agent's current + recent network throughput, derived from the index.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentNet {
    pub agent_id: String,
    pub rx_bps: u64,            // latest sample
    pub tx_bps: u64,            // latest sample
    pub history: Vec<u64>,      // last N (rx+tx) totals, oldest→newest, for the sparkline
    pub last_ts: u64,          // newest sample ts (for stale detection)
}

/// Scan the index for `metric` events, group by agent_id, newest-last.
/// `window` = how many recent samples to keep for the sparkline (e.g. 16).
pub fn agent_net_stats(index: &EventIndex, window: usize) -> Vec<AgentNet> {
    use crate::event_index::EventQuery;
    let mut events = index.query(&EventQuery {
        kinds: vec!["metric".into()],
        limit: usize::MAX,
        ..Default::default()
    });
    // query() returns newest-first, so reverse to process oldest-to-newest
    events.reverse();

    let mut map: std::collections::BTreeMap<String, AgentNet> = std::collections::BTreeMap::new();
    for e in events {
        let Some(ref agent_id) = e.agent_id else {
            continue;
        };
        let rx = e.raw.get("net_rx_bps").and_then(|v| v.as_u64()).unwrap_or(0);
        let tx = e.raw.get("net_tx_bps").and_then(|v| v.as_u64()).unwrap_or(0);
        let ts = e.ts;

        if let Some(net) = map.get_mut(agent_id) {
            net.rx_bps = rx;
            net.tx_bps = tx;
            net.last_ts = ts;
            net.history.push(rx + tx);
            if net.history.len() > window {
                net.history.remove(0);
            }
        } else {
            map.insert(
                agent_id.clone(),
                AgentNet {
                    agent_id: agent_id.clone(),
                    rx_bps: rx,
                    tx_bps: tx,
                    history: vec![rx + tx],
                    last_ts: ts,
                },
            );
        }
    }
    map.into_values().collect()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FleetContainer {
    pub name: String,
    pub provider: String,
    pub workspace: String,
    pub state: String,
    pub uptime: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FleetRow {
    pub agent_id: String,
    pub provider: String,
    pub workspace: String,
    pub state: String,
    pub uptime: u64,
    pub cpu_pct: f64,
    pub mem_used_kb: u64,
    pub net_rx_bps: u64,
    pub net_tx_bps: u64,
    pub last_ts: u64,
}

pub fn fleet_rows(index: &EventIndex, containers: &[FleetContainer]) -> Vec<FleetRow> {
    use crate::event_index::EventQuery;
    let events = index.query(&EventQuery {
        kinds: vec!["metric".into()],
        limit: usize::MAX,
        ..Default::default()
    });

    let mut newest_metrics = std::collections::HashMap::new();
    for e in events {
        if let Some(ref agent_id) = e.agent_id {
            if !newest_metrics.contains_key(agent_id) {
                newest_metrics.insert(agent_id.clone(), e);
            }
        }
    }

    containers
        .iter()
        .map(|c| {
            let metric = newest_metrics.get(&c.name);
            let cpu_pct = metric
                .and_then(|e| e.raw.get("cpu_pct").and_then(|v| v.as_f64()))
                .unwrap_or(0.0);
            let mem_used_kb = metric
                .and_then(|e| e.raw.get("mem_used_kb").and_then(|v| v.as_u64()))
                .unwrap_or(0);
            let net_rx_bps = metric
                .and_then(|e| e.raw.get("net_rx_bps").and_then(|v| v.as_u64()))
                .unwrap_or(0);
            let net_tx_bps = metric
                .and_then(|e| e.raw.get("net_tx_bps").and_then(|v| v.as_u64()))
                .unwrap_or(0);
            let last_ts = metric.map(|e| e.ts).unwrap_or(0);

            FleetRow {
                agent_id: c.name.clone(),
                provider: c.provider.clone(),
                workspace: c.workspace.clone(),
                state: c.state.clone(),
                uptime: c.uptime,
                cpu_pct,
                mem_used_kb,
                net_rx_bps,
                net_tx_bps,
                last_ts,
            }
        })
        .collect()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelemetryHealth {
    pub events_path: String,
    pub indexed: usize,
    pub newest_ts: u64,
    pub lag_secs: u64,
    pub tagged_ratio: f64,
}

pub fn health(index: &EventIndex, path: &Path) -> TelemetryHealth {
    use crate::event_index::EventQuery;
    let all_events = index.query(&EventQuery {
        limit: usize::MAX,
        ..Default::default()
    });
    let indexed = all_events.len();
    let newest_ts = all_events.first().map(|e| e.ts).unwrap_or(0);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let lag_secs = if newest_ts == 0 || now < newest_ts {
        0
    } else {
        now - newest_ts
    };
    let tagged = all_events.iter().filter(|e| e.agent_id.is_some()).count();
    let tagged_ratio = if indexed == 0 {
        1.0
    } else {
        tagged as f64 / indexed as f64
    };

    TelemetryHealth {
        events_path: path.to_string_lossy().to_string(),
        indexed,
        newest_ts,
        lag_secs,
        tagged_ratio,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event_index::EventIndex;
    use serde_json::json;

    #[test]
    fn test_agent_net_stats() {
        let mut index = EventIndex::new(10);
        index.ingest_value(json!({
            "kind": "metric",
            "agent_id": "agent-1",
            "net_rx_bps": 100,
            "net_tx_bps": 50,
            "ts": 1000
        }));
        index.ingest_value(json!({
            "kind": "metric",
            "agent_id": "agent-1",
            "net_rx_bps": 120,
            "net_tx_bps": 60,
            "ts": 1005
        }));
        // Untagged event (should be skipped)
        index.ingest_value(json!({
            "kind": "metric",
            "net_rx_bps": 200,
            "net_tx_bps": 100,
            "ts": 1006
        }));
        index.ingest_value(json!({
            "kind": "metric",
            "agent_id": "agent-2",
            "net_rx_bps": 10,
            "net_tx_bps": 20,
            "ts": 1007
        }));

        let stats = agent_net_stats(&index, 2);
        assert_eq!(stats.len(), 2);
        assert_eq!(stats[0].agent_id, "agent-1");
        assert_eq!(stats[0].rx_bps, 120);
        assert_eq!(stats[0].tx_bps, 60);
        assert_eq!(stats[0].last_ts, 1005);
        assert_eq!(stats[0].history, vec![150, 180]); // 100+50, 120+60

        assert_eq!(stats[1].agent_id, "agent-2");
        assert_eq!(stats[1].rx_bps, 10);
        assert_eq!(stats[1].tx_bps, 20);
        assert_eq!(stats[1].last_ts, 1007);
        assert_eq!(stats[1].history, vec![30]);
    }

    #[test]
    fn test_fleet_rows() {
        let mut index = EventIndex::new(10);
        index.ingest_value(json!({
            "kind": "metric",
            "agent_id": "agent-1",
            "cpu_pct": 5.0,
            "mem_used_kb": 1024,
            "net_rx_bps": 50,
            "net_tx_bps": 50,
            "ts": 1000
        }));
        index.ingest_value(json!({
            "kind": "metric",
            "agent_id": "agent-1",
            "cpu_pct": 10.0,
            "mem_used_kb": 2048,
            "net_rx_bps": 60,
            "net_tx_bps": 60,
            "ts": 1005
        }));

        let containers = vec![
            FleetContainer {
                name: "agent-1".to_string(),
                provider: "docker".to_string(),
                workspace: "/work".to_string(),
                state: "running".to_string(),
                uptime: 100,
            },
            FleetContainer {
                name: "agent-2".to_string(),
                provider: "docker".to_string(),
                workspace: "/work2".to_string(),
                state: "stopped".to_string(),
                uptime: 0,
            },
        ];

        let rows = fleet_rows(&index, &containers);
        assert_eq!(rows.len(), 2);

        assert_eq!(rows[0].agent_id, "agent-1");
        assert_eq!(rows[0].cpu_pct, 10.0);
        assert_eq!(rows[0].mem_used_kb, 2048);
        assert_eq!(rows[0].last_ts, 1005);

        assert_eq!(rows[1].agent_id, "agent-2");
        assert_eq!(rows[1].cpu_pct, 0.0);
        assert_eq!(rows[1].mem_used_kb, 0);
        assert_eq!(rows[1].last_ts, 0);
    }

    #[test]
    fn test_health() {
        let mut index = EventIndex::new(10);
        index.ingest_value(json!({
            "kind": "metric",
            "agent_id": "agent-1",
            "ts": 1000
        }));
        index.ingest_value(json!({
            "kind": "metric",
            "ts": 1005
        }));

        let h = health(&index, Path::new("/dummy/path"));
        assert_eq!(h.events_path, "/dummy/path");
        assert_eq!(h.indexed, 2);
        assert_eq!(h.newest_ts, 1005);
        assert_eq!(h.tagged_ratio, 0.5);
    }

    #[test]
    fn test_limit_bug_regression() {
        let mut index = EventIndex::new(600);
        for i in 0..501 {
            index.ingest_value(json!({
                "kind": "metric",
                "agent_id": "agent-1",
                "net_rx_bps": 10,
                "net_tx_bps": 10,
                "ts": 1000 + i
            }));
        }
        index.ingest_value(json!({
            "kind": "metric",
            "agent_id": "agent-2",
            "net_rx_bps": 20,
            "net_tx_bps": 20,
            "ts": 2000
        }));

        let stats = agent_net_stats(&index, 16);
        assert_eq!(stats.len(), 2);
        assert!(stats.iter().any(|s| s.agent_id == "agent-2"));
    }

    #[test]
    fn test_telemetry_state_poisoning() {
        let state = TelemetryState::new(10);
        let index_clone = state.index.clone();
        let _ = std::panic::catch_unwind(move || {
            let _guard = index_clone.lock().unwrap();
            panic!("poisoning");
        });

        // This call should not panic because it recovers the poisoned lock
        state.refresh();
    }
}
