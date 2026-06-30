//! LOGPANE EPIC 2 — event index. Turns the monitor's append-only event stream
//! (`events.jsonl`, produced by EPIC 1's collectors + the FS watcher + pulse)
//! into a queryable, faceted in-memory store — the search engine the Splunk-style
//! UI (EPIC 3) reads.
//!
//! It's generic over [`crate::monitor::MonitorEvent`]'s JSON shape: every event
//! carries a `kind` tag and a `ts`, and all nested string values are folded into
//! a lowercase search blob. New event types are therefore indexed and searchable
//! with no changes here. Backed by a bounded ring (newest-wins) so a long-running
//! agent can't grow it without limit.

use std::collections::BTreeMap;
use std::path::Path;

/// One indexed event: the original JSON plus the extracted facets used for
/// filtering (`ts`, `kind`) and a precomputed lowercase search blob.
#[derive(Debug, Clone)]
pub struct IndexedEvent {
    pub ts: u64,
    pub kind: String,
    /// Producing agent's identity (NEMESIS8_AGENT_ID), when the event is tagged.
    pub agent_id: Option<String>,
    pub raw: serde_json::Value,
    search: String,
}

impl IndexedEvent {
    fn from_value(v: serde_json::Value) -> Self {
        let ts = v.get("ts").and_then(|t| t.as_u64()).unwrap_or(0);
        let kind = v
            .get("kind")
            .and_then(|k| k.as_str())
            .unwrap_or("unknown")
            .to_string();
        let agent_id = v
            .get("agent_id")
            .and_then(|a| a.as_str())
            .map(|s| s.to_string());
        let mut blob = String::new();
        collect_strings(&v, &mut blob);
        Self { ts, kind, agent_id, raw: v, search: blob.to_lowercase() }
    }
}

/// A search over the index. All fields are AND-combined; empty/None fields don't
/// constrain. Mirrors a Splunk-style query: kind facets + time window + free text.
#[derive(Debug, Clone, Default)]
pub struct EventQuery {
    /// Restrict to these kinds (empty = all kinds).
    pub kinds: Vec<String>,
    /// `ts >= since` when set.
    pub since: Option<u64>,
    /// `ts <= until` when set.
    pub until: Option<u64>,
    /// Case-insensitive substring over the event's string fields.
    pub text: Option<String>,
    /// Max results returned (0 → [`DEFAULT_LIMIT`]).
    pub limit: usize,
}

pub const DEFAULT_LIMIT: usize = 500;

/// A bounded, newest-wins index of monitor events.
pub struct EventIndex {
    events: Vec<IndexedEvent>,
    cap: usize,
}

impl EventIndex {
    pub fn new(cap: usize) -> Self {
        Self { events: Vec::new(), cap: cap.max(1) }
    }

    /// Ingest a parsed event JSON value, evicting the oldest if over capacity.
    pub fn ingest_value(&mut self, v: serde_json::Value) {
        self.events.push(IndexedEvent::from_value(v));
        if self.events.len() > self.cap {
            let overflow = self.events.len() - self.cap;
            self.events.drain(0..overflow);
        }
    }

    /// Ingest one JSONL line. Returns false (and ignores) if it isn't valid JSON.
    pub fn ingest_line(&mut self, line: &str) -> bool {
        let line = line.trim();
        if line.is_empty() {
            return false;
        }
        match serde_json::from_str::<serde_json::Value>(line) {
            Ok(v) => {
                self.ingest_value(v);
                true
            }
            Err(_) => false,
        }
    }

    /// Build an index from the TAIL of an `events.jsonl` (best-effort; malformed
    /// lines skipped). Only the last ~`cap` lines are read, so a multi-GB stream
    /// (the fs watcher alone produces millions) never gets pulled wholesale into
    /// memory. Missing/unreadable file → empty index.
    pub fn load_jsonl(path: impl AsRef<Path>, cap: usize) -> Self {
        let mut idx = Self::new(cap);
        let _ = idx.read_tail(path);
        idx
    }

    fn read_tail(&mut self, path: impl AsRef<Path>) -> std::io::Result<()> {
        use std::io::{Read, Seek, SeekFrom};
        let mut f = std::fs::File::open(path)?;
        let size = f.metadata()?.len();
        // ~256 B/line heuristic for the read window, clamped to 32 MiB — enough
        // for the last `cap` events without reading the whole file.
        let window = (self.cap as u64).saturating_mul(256).min(32 * 1024 * 1024);
        let start = size.saturating_sub(window);
        f.seek(SeekFrom::Start(start))?;
        let mut bytes = Vec::new();
        f.read_to_end(&mut bytes)?;
        let text = String::from_utf8_lossy(&bytes);
        // If we began mid-file, the first line is partial — drop it.
        let body: &str = if start > 0 {
            text.find('\n').map(|i| &text[i + 1..]).unwrap_or("")
        } else {
            &text
        };
        for line in body.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                self.events.push(IndexedEvent::from_value(v));
            }
        }
        if self.events.len() > self.cap {
            let overflow = self.events.len() - self.cap;
            self.events.drain(0..overflow);
        }
        Ok(())
    }

    /// Run a query, newest-first, capped at `limit`.
    pub fn query(&self, q: &EventQuery) -> Vec<&IndexedEvent> {
        let needle = q.text.as_ref().map(|t| t.to_lowercase());
        let mut hits: Vec<&IndexedEvent> = self
            .events
            .iter()
            .filter(|e| q.kinds.is_empty() || q.kinds.iter().any(|k| k == &e.kind))
            .filter(|e| q.since.map(|s| e.ts >= s).unwrap_or(true))
            .filter(|e| q.until.map(|u| e.ts <= u).unwrap_or(true))
            .filter(|e| needle.as_ref().map(|n| e.search.contains(n)).unwrap_or(true))
            .collect();
        // Newest first (Splunk default). Stable so equal-ts keep arrival order.
        hits.sort_by(|a, b| b.ts.cmp(&a.ts));
        let limit = if q.limit == 0 { DEFAULT_LIMIT } else { q.limit };
        hits.truncate(limit);
        hits
    }

    /// Per-kind counts across the whole index — drives the UI's facet sidebar.
    pub fn facets(&self) -> BTreeMap<String, usize> {
        let mut out = BTreeMap::new();
        for e in &self.events {
            *out.entry(e.kind.clone()).or_insert(0) += 1;
        }
        out
    }

    pub fn len(&self) -> usize {
        self.events.len()
    }

    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }
}

/// Recursively fold every string value in a JSON tree into `out` (space-joined).
/// Object keys and numbers are skipped — search targets the human-readable
/// content (log lines, paths, status messages), while structured filtering goes
/// through `kind`/`ts`.
fn collect_strings(v: &serde_json::Value, out: &mut String) {
    match v {
        serde_json::Value::String(s) => {
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(s);
        }
        serde_json::Value::Array(a) => {
            for item in a {
                collect_strings(item, out);
            }
        }
        serde_json::Value::Object(o) => {
            for val in o.values() {
                collect_strings(val, out);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn idx() -> EventIndex {
        let mut i = EventIndex::new(100);
        i.ingest_value(json!({"kind":"log_line","ts":10,"path":"/workspace/app.log","line":"ERROR boom"}));
        i.ingest_value(json!({"kind":"log_line","ts":20,"path":"/workspace/app.log","line":"all good"}));
        i.ingest_value(json!({"kind":"metric","ts":15,"cpu_pct":42.0,"load1":0.5}));
        i.ingest_value(json!({"kind":"fs","ts":25,"path":"/workspace/src/main.rs","kind_detail":"modified"}));
        i
    }

    #[test]
    fn filters_by_kind() {
        let i = idx();
        let hits = i.query(&EventQuery { kinds: vec!["log_line".into()], ..Default::default() });
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().all(|e| e.kind == "log_line"));
    }

    #[test]
    fn newest_first_and_text_search() {
        let i = idx();
        // text search is case-insensitive over string fields
        let hits = i.query(&EventQuery { text: Some("error".into()), ..Default::default() });
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].ts, 10);

        // no filter → all 4, newest ts first
        let all = i.query(&EventQuery::default());
        assert_eq!(all.len(), 4);
        assert_eq!(all[0].ts, 25);
        assert_eq!(all[3].ts, 10);
    }

    #[test]
    fn time_window_and_limit() {
        let i = idx();
        let hits = i.query(&EventQuery { since: Some(15), until: Some(20), ..Default::default() });
        assert_eq!(hits.len(), 2); // ts 15 and 20
        let one = i.query(&EventQuery { limit: 1, ..Default::default() });
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].ts, 25);
    }

    #[test]
    fn facets_count_by_kind() {
        let f = idx().facets();
        assert_eq!(f.get("log_line"), Some(&2));
        assert_eq!(f.get("metric"), Some(&1));
        assert_eq!(f.get("fs"), Some(&1));
    }

    #[test]
    fn ring_evicts_oldest() {
        let mut i = EventIndex::new(2);
        i.ingest_value(json!({"kind":"a","ts":1}));
        i.ingest_value(json!({"kind":"b","ts":2}));
        i.ingest_value(json!({"kind":"c","ts":3}));
        assert_eq!(i.len(), 2);
        let all = i.query(&EventQuery::default());
        assert_eq!(all[0].ts, 3);
        assert_eq!(all[1].ts, 2); // ts 1 evicted
    }

    #[test]
    fn load_jsonl_tails_to_cap() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("events.jsonl");
        let mut f = std::fs::File::create(&p).unwrap();
        for i in 0..50u64 {
            writeln!(f, r#"{{"kind":"fs","ts":{i},"path":"/x"}}"#).unwrap();
        }
        // cap=1 → tiny window forces a tail read; ring keeps only the newest.
        let idx = EventIndex::load_jsonl(&p, 1);
        assert_eq!(idx.len(), 1);
        assert_eq!(idx.query(&EventQuery::default())[0].ts, 49);
    }

    #[test]
    #[ignore] // diagnostic: run against the live host stream on demand
    fn debug_load_real_stream() {
        let p = dirs::home_dir()
            .unwrap()
            .join(".nemesis8/home/.monitor/events.jsonl");
        let idx = EventIndex::load_jsonl(&p, 50_000);
        eprintln!("DIAG loaded {} events; facets {:?}", idx.len(), idx.facets());
        let recent = idx.query(&EventQuery { limit: 3, ..Default::default() });
        for e in recent {
            eprintln!("DIAG  ts={} kind={}", e.ts, e.kind);
        }
    }

    #[test]
    fn parses_and_searches_agent_id() {
        let mut i = EventIndex::new(10);
        i.ingest_value(json!({"kind":"metric","ts":1,"agent_id":"n8-swift-hare","cpu_pct":1.0}));
        i.ingest_value(json!({"kind":"fs","ts":2,"path":"/x"}));
        let all = i.query(&EventQuery::default());
        let m = all.iter().find(|e| e.kind == "metric").unwrap();
        assert_eq!(m.agent_id.as_deref(), Some("n8-swift-hare"));
        assert_eq!(all.iter().find(|e| e.kind == "fs").unwrap().agent_id, None);
        // the agent name is also free-text searchable (folded into the blob)
        let hits = i.query(&EventQuery { text: Some("swift-hare".into()), ..Default::default() });
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].ts, 1);
    }

    #[test]
    fn skips_malformed_lines() {
        let mut i = EventIndex::new(10);
        assert!(i.ingest_line(r#"{"kind":"fs","ts":1,"path":"x"}"#));
        assert!(!i.ingest_line("not json"));
        assert!(!i.ingest_line(""));
        assert_eq!(i.len(), 1);
    }
}
