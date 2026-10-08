use crate::event_index::EventIndex;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

#[derive(Clone)]
pub struct TelemetryState {
    pub index: Arc<Mutex<EventIndex>>,
    /// The `.monitor` directory re-scanned each refresh for every event file.
    /// Containers no longer share one `events.jsonl`: each writes its own
    /// `events.<agent_id>.jsonl` (set via `NEMESIS8_EVENTS_FILE`) so concurrent
    /// cross-container appends can't interleave into corrupt "glued" lines.
    pub monitor_dir: PathBuf,
    /// The legacy shared file + its rotation. Kept as the health label and the
    /// first-load ordering anchor; still tailed for any non-containerised or
    /// pre-upgrade writer that targets it.
    pub events_path: PathBuf,
    pub sibling_path: PathBuf,
    /// (mtime, size) of every event file seen last refresh, keyed by path.
    /// Replaces the four fixed main/sibling guards: change detection, the
    /// first-load flag (`is_empty`), and per-file growth cursors all key off
    /// this map so an arbitrary number of per-agent files is handled uniformly.
    pub file_states: Arc<Mutex<std::collections::HashMap<PathBuf, (Option<std::time::SystemTime>, u64)>>>,
    pub cap: usize,
    pub broadcast_tx: tokio::sync::broadcast::Sender<serde_json::Value>,
    pub token_cache: Arc<Mutex<std::collections::HashMap<String, (std::time::SystemTime, u64)>>>,
    pub net_cache: Arc<Mutex<std::collections::HashMap<String, (u64, u64, u64)>>>,
    /// Lume-backed SEARCH corpus (#79). The ring above serves the live jobs;
    /// any query with `q` routes here for ranked, full-history retrieval.
    pub event_store: Arc<Mutex<crate::event_store::EventStore>>,
    /// Session-transcript tailer synthesizing `tool_call` events (who called
    /// what, with which params) into the pipeline. Polled with the fleet join.
    pub tool_tailer: Arc<Mutex<crate::tool_events::ToolCallTailer>>,
    /// Tool-call tailer for providers whose transcript is a shared sqlite db
    /// (opencode, hermes) rather than JSONL. Polled alongside `tool_tailer`.
    pub sqlite_tool_tailer: Arc<Mutex<crate::tool_events::SqliteToolTailer>>,
    /// Host-synthesized events that are NOT in events.jsonl (tool_call from the
    /// tailer). refresh() rebuilds the ring from disk every ~1s, which would
    /// wipe anything not on disk — so these are buffered here and re-injected
    /// into the ring after every rebuild. Bounded (last SYNTHETIC_CAP).
    pub synthetic: Arc<Mutex<std::collections::VecDeque<serde_json::Value>>>,
}

/// Cap on the retained synthetic-event buffer (tool_call events survive ring
/// rebuilds up to this many).
const SYNTHETIC_CAP: usize = 2000;

impl TelemetryState {
    pub fn new(cap: usize) -> Self {
        let home = crate::paths::data_home();
        let monitor_dir = home.join(".monitor");
        let events_path = monitor_dir.join("events.jsonl");
        let sibling_path = monitor_dir.join("events.jsonl.1");
        let (tx, _) = tokio::sync::broadcast::channel(1024);
        Self {
            index: Arc::new(Mutex::new(EventIndex::new(cap))),
            monitor_dir,
            events_path,
            sibling_path,
            file_states: Arc::new(Mutex::new(std::collections::HashMap::new())),
            cap,
            broadcast_tx: tx,
            token_cache: Arc::new(Mutex::new(std::collections::HashMap::new())),
            net_cache: Arc::new(Mutex::new(std::collections::HashMap::new())),
            event_store: Arc::new(Mutex::new(crate::event_store::EventStore::new())),
            tool_tailer: Arc::new(Mutex::new(crate::tool_events::ToolCallTailer::new())),
            sqlite_tool_tailer: Arc::new(Mutex::new(crate::tool_events::SqliteToolTailer::new())),
            synthetic: Arc::new(Mutex::new(std::collections::VecDeque::new())),
        }
    }

    /// Ingest events synthesized host-side (not written to events.jsonl) — the
    /// tailer's `tool_call` events. Adds each to the live ring, the search
    /// store, and the SSE broadcast ONCE, and buffers it so refresh() can
    /// re-inject it into the ring after the next disk rebuild. Lock order
    /// index → store → synthetic (matches refresh()).
    pub fn ingest_synthetic(&self, events: Vec<serde_json::Value>) {
        if events.is_empty() {
            return;
        }
        let mut ring = self.index.lock().unwrap_or_else(|p| p.into_inner());
        let mut store = self.event_store.lock().unwrap_or_else(|p| p.into_inner());
        let mut buf = self.synthetic.lock().unwrap_or_else(|p| p.into_inner());
        for e in events {
            ring.ingest_value(e.clone());
            store.ingest_value(e.clone());
            let _ = self.broadcast_tx.send(e.clone());
            buf.push_back(e);
            while buf.len() > SYNTHETIC_CAP {
                buf.pop_front();
            }
        }
    }

    pub fn refresh(&self) {
        // Every event file in the monitor dir: the legacy shared file + its
        // rotation, plus one `events.<agent_id>.jsonl` (+`.1`) per container.
        // Siblings (`.1`, the OLDER events) are ordered before their live file
        // so, when the total exceeds the ring cap, the newer events win.
        let files = discover_event_files(&self.monitor_dir, &self.events_path, &self.sibling_path);

        // Current (mtime, size) for each file; absent files read as (None, 0).
        let current: Vec<(PathBuf, Option<std::time::SystemTime>, u64)> = files
            .iter()
            .map(|p| {
                let (mt, sz) = std::fs::metadata(p)
                    .map(|m| (Some(m.modified().unwrap_or(std::time::UNIX_EPOCH)), m.len()))
                    .unwrap_or((None, 0));
                (p.clone(), mt, sz)
            })
            .collect();

        let mut states = self.file_states.lock().unwrap_or_else(|p| p.into_inner());
        let first_load = states.is_empty();

        // Detect change and collect per-file growth ranges for the search store
        // + SSE broadcast. A file that newly appears after start-up grows from
        // 0 so its whole content is streamed.
        let mut changed = false;
        let mut growth: Vec<(PathBuf, u64, u64)> = Vec::new();
        for (p, mt, sz) in &current {
            match states.get(p) {
                Some((omt, osz)) => {
                    if omt != mt || *osz != *sz {
                        changed = true;
                    }
                    if *sz > *osz {
                        growth.push((p.clone(), *osz, *sz));
                    } else if *sz < *osz {
                        // Rotation/truncation: re-stream from the start.
                        if *sz > 0 {
                            growth.push((p.clone(), 0, *sz));
                        }
                    }
                }
                None => {
                    changed = true;
                    if *sz > 0 && !first_load {
                        growth.push((p.clone(), 0, *sz));
                    }
                }
            }
        }
        // A file that disappeared (dir pruned) also counts as a change so the
        // ring rebuilds without it.
        if states.keys().any(|k| !current.iter().any(|(p, _, _)| p == k)) {
            changed = true;
        }

        if !changed {
            return;
        }

        // Rebuild the ring from every file's tail: all siblings first (older),
        // then all live files.
        let is_sibling = |p: &Path| {
            p.to_string_lossy().ends_with(".jsonl.1")
        };
        let mut new_index = EventIndex::new(self.cap);
        for (p, mt, _) in &current {
            if mt.is_some() && is_sibling(p) {
                let _ = read_tail_into_index(&mut new_index, p, self.cap);
            }
        }
        for (p, mt, _) in &current {
            if mt.is_some() && !is_sibling(p) {
                let _ = read_tail_into_index(&mut new_index, p, self.cap);
            }
        }

        let mut index_guard = self.index.lock().unwrap_or_else(|p| p.into_inner());
        *index_guard = new_index;

        // Re-inject host-synthesized events (tool_call) that the fresh
        // disk-built index doesn't contain — otherwise they'd vanish within a
        // second of being synthesized.
        {
            let buf = self.synthetic.lock().unwrap_or_else(|p| p.into_inner());
            for e in buf.iter() {
                index_guard.ingest_value(e.clone());
            }
        }

        if first_load {
            // First refresh after start: bulk-load the whole history into the
            // search store (siblings first — the OLDER events — then live).
            // One base build per file, never O(n²).
            let mut store = self.event_store.lock().unwrap_or_else(|p| p.into_inner());
            if store.is_empty() {
                let mut loaded = 0usize;
                for (p, mt, _) in &current {
                    if mt.is_some() && is_sibling(p) {
                        loaded += store.ingest_file(p).unwrap_or(0);
                    }
                }
                for (p, mt, _) in &current {
                    if mt.is_some() && !is_sibling(p) {
                        loaded += store.ingest_file(p).unwrap_or(0);
                    }
                }
                if loaded > 0 {
                    tracing::info!(docs = loaded, "event search store loaded (lume)");
                }
            }
        } else if !growth.is_empty() {
            // Incremental: stream each file's new byte range into the search
            // store and out over SSE. Glued JSON objects (two containers' or a
            // writer-vs-monitor race wrote without a clean newline) are split
            // defensively so neither event is lost.
            let mut store = self.event_store.lock().unwrap_or_else(|p| p.into_inner());
            for (p, start, end) in &growth {
                if let Ok(chunk) = read_range(p, *start, *end) {
                    for line in chunk.lines() {
                        for v in parse_jsonl_fragment(line) {
                            store.ingest_value(v.clone());
                            let _ = self.broadcast_tx.send(v);
                        }
                    }
                }
            }
        }

        *states = current
            .into_iter()
            .map(|(p, mt, sz)| (p, (mt, sz)))
            .collect();
    }
}

/// Every event file the gateway should tail: the legacy shared `events.jsonl`
/// (+ its `.1` rotation) plus one `events.<agent_id>.jsonl` (+`.1`) per
/// container. `main`/`sibling` are always included even when absent so a
/// freshly-started gateway behaves exactly as before until containers appear.
fn discover_event_files(dir: &Path, main: &Path, sibling: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = vec![main.to_path_buf(), sibling.to_path_buf()];
    if let Ok(rd) = std::fs::read_dir(dir) {
        for entry in rd.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with("events") && (name.ends_with(".jsonl") || name.ends_with(".jsonl.1"))
            {
                let p = entry.path();
                if !v.contains(&p) {
                    v.push(p);
                }
            }
        }
    }
    v.sort();
    v.dedup();
    v
}

/// Parse one physical line into the JSON objects it holds. Normally that's
/// exactly one, but concurrent appends to a bind-mounted file can glue two
/// objects onto a single line (`{..}{..}`); a streaming deserializer pulls each
/// out so neither is dropped. Stops at the first unparseable remainder.
fn parse_jsonl_fragment(line: &str) -> Vec<serde_json::Value> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let de = serde_json::Deserializer::from_str(trimmed).into_iter::<serde_json::Value>();
    for v in de {
        match v {
            Ok(val) => out.push(val),
            Err(_) => break,
        }
    }
    out
}

fn read_range(path: &Path, start: u64, end: u64) -> std::io::Result<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path)?;
    f.seek(SeekFrom::Start(start))?;
    let mut buf = vec![0u8; (end - start) as usize];
    f.read_exact(&mut buf)?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
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
        for v in parse_jsonl_fragment(line) {
            index.ingest_value(v);
        }
    }
    Ok(())
}

/// One agent's current + recent network throughput, derived from the index.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentNet {
    pub agent_id: String,
    pub rx_bps: u64,       // latest sample
    pub tx_bps: u64,       // latest sample
    pub history: Vec<u64>, // last N (rx+tx) totals, oldest→newest, for the sparkline
    pub last_ts: u64,      // newest sample ts (for stale detection)
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
        let rx = e
            .raw
            .get("net_rx_bps")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let tx = e
            .raw
            .get("net_tx_bps")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
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
    pub model: String,
    pub workspace: String,
    pub state: String,
    pub uptime: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FleetRow {
    pub agent_id: String,
    pub provider: String,
    pub model: String,
    pub workspace: String,
    pub state: String,
    pub uptime: u64,
    pub cpu_pct: f64,
    pub mem_used_kb: u64,
    pub net_rx_bps: u64,
    pub net_tx_bps: u64,
    pub tok_s: u64,
    pub last_ts: u64,
    /// Host path to the agent's newest session transcript, when resolvable.
    /// Additive — lets the Hyperia telemetry pusher read cumulative token
    /// usage without re-listing sessions. Clients tolerate the extra field.
    #[serde(default)]
    pub session_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeMetrics {
    pub cpu_pct: f64,
    pub mem_used_kb: u64,
    pub net_rx_bps: u64,
    pub net_tx_bps: u64,
}

pub fn fleet_rows(
    index: &EventIndex,
    containers: &[FleetContainer],
    sessions: &[crate::session::SessionInfo],
    token_cache: &Mutex<std::collections::HashMap<String, (std::time::SystemTime, u64)>>,
    runtime_metrics: &std::collections::HashMap<String, RuntimeMetrics>,
) -> Vec<FleetRow> {
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
            let fallback_cpu = metric
                .and_then(|e| e.raw.get("cpu_pct").and_then(|v| v.as_f64()))
                .unwrap_or(0.0);
            let fallback_mem = metric
                .and_then(|e| e.raw.get("mem_used_kb").and_then(|v| v.as_u64()))
                .unwrap_or(0);
            let fallback_rx = metric
                .and_then(|e| e.raw.get("net_rx_bps").and_then(|v| v.as_u64()))
                .unwrap_or(0);
            let fallback_tx = metric
                .and_then(|e| e.raw.get("net_tx_bps").and_then(|v| v.as_u64()))
                .unwrap_or(0);
            let last_ts = metric.map(|e| e.ts).unwrap_or(0);

            // Use runtime true metrics if present; fallback to events if None/failed
            let (cpu_pct, mem_used_kb, net_rx_bps, net_tx_bps) = if let Some(rm) = runtime_metrics.get(&c.name) {
                (rm.cpu_pct, rm.mem_used_kb, rm.net_rx_bps, rm.net_tx_bps)
            } else {
                (fallback_cpu, fallback_mem, fallback_rx, fallback_tx)
            };

            // Calculate tokens per second (tok_s) from the newest session
            let newest_session = sessions
                .iter()
                .filter(|s| {
                    s.provider.as_deref() == Some(c.provider.as_str())
                        && s.workspace.as_deref() == Some(c.workspace.as_str())
                })
                .max_by(|a, b| a.modified.cmp(&b.modified));

            let mut tok_s = 0;
            if let Some(s) = newest_session {
                let session_path = &s.path;
                if let Ok(meta) = std::fs::metadata(session_path) {
                    if let Ok(mtime) = meta.modified() {
                        let mut cache = token_cache.lock().unwrap_or_else(|p| p.into_inner());
                        if let Some(&(cached_mtime, cached_tok_s)) = cache.get(session_path) {
                            if cached_mtime == mtime {
                                tok_s = cached_tok_s;
                            } else {
                                tok_s = calculate_tokens_per_sec(session_path);
                                cache.insert(session_path.clone(), (mtime, tok_s));
                            }
                        } else {
                            tok_s = calculate_tokens_per_sec(session_path);
                            cache.insert(session_path.clone(), (mtime, tok_s));
                        }
                    }
                }
            }

            FleetRow {
                agent_id: c.name.clone(),
                provider: c.provider.clone(),
                model: c.model.clone(),
                workspace: c.workspace.clone(),
                state: c.state.clone(),
                uptime: c.uptime,
                cpu_pct,
                mem_used_kb,
                net_rx_bps,
                net_tx_bps,
                tok_s,
                last_ts,
                session_path: newest_session.map(|s| s.path.clone()),
            }
        })
        .collect()
}

/// Input/output tokens from turns in `path` strictly newer than `since_epoch`
/// (unix secs), for the Hyperia telemetry push. Returns (new_input, new_output,
/// new_cursor). Reads only the tail — an agent completes few turns per 3s push.
///
/// Handles both transcript dialects (verified against live sessions 2026-08-20):
///   - claude:  {type:assistant, message:{usage:{input_tokens, output_tokens}}}
///   - codex:   {type:event_msg, payload:{type:token_count,
///                info:{last_token_usage:{input_tokens, output_tokens}}}}
/// Both carry PER-TURN usage, so summing new turns is correct for each.
/// Providers with other formats (grok, agy protobuf) still report 0.
pub fn token_delta_since(path: &str, since_epoch: i64) -> (u64, u64, i64) {
    let lines = match crate::transcript::read_tail(path, 200) {
        Ok(l) => l,
        Err(_) => return (0, 0, since_epoch),
    };
    let mut new_in = 0u64;
    let mut new_out = 0u64;
    let mut cursor = since_epoch;
    for line in lines {
        let Ok(val) = serde_json::from_str::<serde_json::Value>(&line) else { continue };
        let epoch = val
            .get("timestamp")
            .and_then(|t| t.as_str())
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|d| d.timestamp())
            .unwrap_or(0);
        if epoch <= since_epoch {
            continue;
        }
        // claude turn
        let claude = (val.get("type").and_then(|t| t.as_str()) == Some("assistant"))
            .then(|| val.get("message").and_then(|m| m.get("usage")))
            .flatten();
        // codex token_count event
        let codex = (val.get("type").and_then(|t| t.as_str()) == Some("event_msg"))
            .then(|| val.get("payload"))
            .flatten()
            .filter(|p| p.get("type").and_then(|t| t.as_str()) == Some("token_count"))
            .and_then(|p| p.get("info"))
            .and_then(|i| i.get("last_token_usage"));
        if let Some(usage) = claude.or(codex) {
            let i = usage.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
            let o = usage.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
            if i > 0 || o > 0 {
                new_in += i;
                new_out += o;
                cursor = cursor.max(epoch);
            }
        }
    }
    (new_in, new_out, cursor)
}

fn calculate_tokens_per_sec(path: &str) -> u64 {
    let lines = match crate::transcript::read_tail(path, 50) {
        Ok(l) => l,
        Err(_) => return 0,
    };

    let mut sum_tokens = 0u64;
    for line in lines {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(&line) {
            if val.get("type").and_then(|t| t.as_str()) == Some("assistant") {
                if let Some(ts_str) = val.get("timestamp").and_then(|t| t.as_str()) {
                    if let Some(age_secs) = parse_ts_secs_ago(ts_str) {
                        if age_secs >= 0 && age_secs <= 60 {
                            if let Some(message) = val.get("message") {
                                if let Some(usage) = message.get("usage") {
                                    if let Some(out_tok) =
                                        usage.get("output_tokens").and_then(|o| o.as_u64())
                                    {
                                        sum_tokens += out_tok;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    (sum_tokens as f64 / 60.0).round() as u64
}

fn parse_ts_secs_ago(ts_str: &str) -> Option<i64> {
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(ts_str) {
        let now = chrono::Utc::now();
        let diff = now.signed_duration_since(dt.with_timezone(&chrono::Utc));
        return Some(diff.num_seconds());
    }
    None
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
                model: "gpt-5.5".to_string(),
                workspace: "/work".to_string(),
                state: "running".to_string(),
                uptime: 100,
            },
            FleetContainer {
                name: "agent-2".to_string(),
                provider: "docker".to_string(),
                model: "".to_string(),
                workspace: "/work2".to_string(),
                state: "stopped".to_string(),
                uptime: 0,
            },
        ];

        let rows = fleet_rows(
            &index,
            &containers,
            &[],
            &Mutex::new(std::collections::HashMap::new()),
            &std::collections::HashMap::new(),
        );
        assert_eq!(rows.len(), 2);

        assert_eq!(rows[0].agent_id, "agent-1");
        assert_eq!(rows[0].model, "gpt-5.5");
        assert_eq!(rows[0].cpu_pct, 10.0);
        assert_eq!(rows[0].mem_used_kb, 2048);
        assert_eq!(rows[0].last_ts, 1005);

        assert_eq!(rows[1].agent_id, "agent-2");
        assert_eq!(rows[1].model, "");
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

    #[test]
    fn glued_lines_split_into_each_object() {
        // A clean single line is one object.
        let one = parse_jsonl_fragment(r#"{"kind":"edit","ts":1}"#);
        assert_eq!(one.len(), 1);
        assert_eq!(one[0]["ts"], 1);

        // Two objects glued with no separator (the cross-writer corruption we
        // now tolerate) yield BOTH, in order.
        let glued = parse_jsonl_fragment(r#"{"kind":"edit","ts":1}{"kind":"fs","ts":2}"#);
        assert_eq!(glued.len(), 2, "glued objects must both be recovered");
        assert_eq!(glued[0]["kind"], "edit");
        assert_eq!(glued[1]["kind"], "fs");

        // A separating space is fine too (the stream deserializer skips it).
        let spaced = parse_jsonl_fragment(r#"{"ts":1} {"ts":2}"#);
        assert_eq!(spaced.len(), 2);

        // A good object followed by unparseable bytes keeps the good one.
        let partial = parse_jsonl_fragment(r#"{"ts":1}{"ts":"#);
        assert_eq!(partial.len(), 1);
        assert_eq!(partial[0]["ts"], 1);

        // Blank / whitespace lines contribute nothing.
        assert!(parse_jsonl_fragment("   ").is_empty());
        assert!(parse_jsonl_fragment("").is_empty());
    }

    #[test]
    fn discover_finds_per_agent_files_and_keeps_legacy() {
        let dir = std::env::temp_dir().join(format!(
            "n8-telemetry-discover-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let main = dir.join("events.jsonl");
        let sibling = dir.join("events.jsonl.1");
        // A per-agent live file + rotation, and non-event files that must be
        // ignored. Note: `main` itself is NOT written to disk here — it must
        // still appear in the result (freshly-started gateway parity).
        std::fs::write(dir.join("events.n8-bold-finch.jsonl"), b"{}\n").unwrap();
        std::fs::write(dir.join("events.n8-bold-finch.jsonl.1"), b"{}\n").unwrap();
        std::fs::write(dir.join("events.n8-calm-otter.jsonl"), b"{}\n").unwrap();
        std::fs::write(dir.join("notes.md"), b"ignore me\n").unwrap();
        std::fs::write(dir.join("state.json"), b"{}\n").unwrap();

        let found = discover_event_files(&dir, &main, &sibling);

        assert!(found.contains(&main), "legacy main always present");
        assert!(found.contains(&sibling), "legacy sibling always present");
        assert!(found.contains(&dir.join("events.n8-bold-finch.jsonl")));
        assert!(found.contains(&dir.join("events.n8-bold-finch.jsonl.1")));
        assert!(found.contains(&dir.join("events.n8-calm-otter.jsonl")));
        assert!(!found.iter().any(|p| p.ends_with("notes.md")));
        assert!(!found.iter().any(|p| p.ends_with("state.json")));
        // No duplicates even though main/sibling are force-included.
        let mut dedup = found.clone();
        dedup.sort();
        dedup.dedup();
        assert_eq!(dedup.len(), found.len());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
