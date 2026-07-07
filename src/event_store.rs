//! Lume-backed event search (#79) — ranked, full-history, on the vendored
//! BM25 engine. This is the SEARCH half of the event pipeline; the in-memory
//! ring (`event_index`) keeps the LIVE jobs it is good at (tail, SSE,
//! sparklines, rollups). Query routing upstream: `q` present → THIS store;
//! no `q` → the ring. One contract, two engines.
//!
//! Lume's `Bm25Index` is build-once, so the store is TWO-TIER:
//!
//!   docs[..base_len]  — covered by a built `Bm25Index` (ranked retrieval)
//!   docs[base_len..]  — the DELTA tail, matched by linear token scan
//!
//! Ingest appends to the delta (cheap); when the delta outgrows
//! `rebuild_threshold` the base is rebuilt over the whole corpus. Search
//! results are the union (base hits ∪ delta matches) post-filtered by
//! kind/agent/since, ordered newest-first — Splunk semantics: time-ordered
//! matches, relevance decides membership, not order.
//!
//! EVERY kind is indexed — including metric and heartbeat: "q=<agent name>"
//! must surface everything a container is doing (its metrics, heartbeats,
//! and file access included). Identity searchability beats corpus thrift.

use lume::bm25::{Bm25Index, Bm25Params, SearchVariant, Section};

/// Rebuild the base index when the delta tail exceeds this many docs.
const DEFAULT_REBUILD_THRESHOLD: usize = 2_000;
/// Hard corpus cap — beyond this the oldest half is dropped (the files
/// themselves rotate at 2×32MB, so this is a backstop, not the retention
/// policy).
const MAX_DOCS: usize = 250_000;

/// One searchable event.
pub struct EventDoc {
    pub ts: u64,
    pub kind: String,
    pub agent: Option<String>,
    pub raw: serde_json::Value,
    /// Flattened searchable text: kind, agent, and every top-level string
    /// field of the event (path, msg, status, kind_detail, …).
    pub text: String,
}

pub struct EventStore {
    docs: Vec<EventDoc>,
    base: Option<Bm25Index>,
    base_len: usize,
    pub rebuild_threshold: usize,
}

impl Default for EventStore {
    fn default() -> Self {
        Self::new()
    }
}

impl EventStore {
    pub fn new() -> Self {
        Self {
            docs: Vec::new(),
            base: None,
            base_len: 0,
            rebuild_threshold: DEFAULT_REBUILD_THRESHOLD,
        }
    }

    pub fn len(&self) -> usize {
        self.docs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.docs.is_empty()
    }

    /// Parse one JSONL line into the corpus. Excluded kinds and unparseable
    /// lines are skipped. Triggers a base rebuild when the delta is due.
    pub fn ingest_line(&mut self, line: &str) {
        if self.push_line(line) {
            self.maybe_rebuild();
        }
    }

    /// Bulk-load a whole events file (startup): push everything, ONE rebuild
    /// at the end — never O(n²).
    pub fn ingest_file(&mut self, path: &std::path::Path) -> std::io::Result<usize> {
        use std::io::BufRead;
        let f = std::fs::File::open(path)?;
        let reader = std::io::BufReader::new(f);
        let mut added = 0usize;
        for line in reader.lines() {
            let Ok(line) = line else { continue };
            if self.push_line(&line) {
                added += 1;
            }
        }
        self.rebuild_base();
        Ok(added)
    }

    fn push_line(&mut self, line: &str) -> bool {
        let line = line.trim();
        if line.is_empty() {
            return false;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            return false;
        };
        let kind = v
            .get("kind")
            .and_then(|k| k.as_str())
            .unwrap_or("event")
            .to_string();
        let agent = v
            .get("agent_id")
            .and_then(|a| a.as_str())
            .map(String::from);
        let ts = v.get("ts").and_then(|t| t.as_u64()).unwrap_or(0);

        // Searchable text: kind + agent + every top-level string value. No
        // per-kind schema to rot — a new event kind is searchable on arrival.
        let mut text = String::with_capacity(64);
        text.push_str(&kind);
        if let Some(a) = &agent {
            text.push(' ');
            text.push_str(a);
        }
        // lume folds "-" by JOINING (n8-noble-otter -> one token), so also
        // index a space-split variant of every hyphen/underscore value —
        // "otter" or "noble otter" must find the container.
        if let Some(a) = &agent {
            if a.contains(['-', '_']) {
                text.push(' ');
                text.push_str(&a.replace(['-', '_'], " "));
            }
        }
        if let Some(obj) = v.as_object() {
            for (k, val) in obj {
                if k == "agent_id" || k == "kind" {
                    continue;
                }
                if let Some(s) = val.as_str() {
                    text.push(' ');
                    text.push_str(s);
                    if s.contains(['-', '_']) {
                        text.push(' ');
                        text.push_str(&s.replace(['-', '_'], " "));
                    }
                }
            }
        }

        self.docs.push(EventDoc {
            ts,
            kind,
            agent,
            raw: v,
            text,
        });

        if self.docs.len() > MAX_DOCS {
            let drop = self.docs.len() / 2;
            self.docs.drain(0..drop);
            self.rebuild_base();
        }
        true
    }

    fn maybe_rebuild(&mut self) {
        if self.docs.len().saturating_sub(self.base_len) >= self.rebuild_threshold {
            self.rebuild_base();
        }
    }

    fn rebuild_base(&mut self) {
        let sections: Vec<Section> = self
            .docs
            .iter()
            .enumerate()
            .map(|(i, d)| Section {
                title: match &d.agent {
                    Some(a) => format!("{} {}", d.kind, a),
                    None => d.kind.clone(),
                },
                body: d.text.clone(),
                line_number: i,
                filename: None,
            })
            .collect();
        self.base = if sections.is_empty() {
            None
        } else {
            Some(Bm25Index::build(sections, None))
        };
        self.base_len = self.docs.len();
    }

    /// Ranked-membership, time-ordered search. `q` decides WHICH docs match
    /// (BM25 over the base + token scan over the delta); the result is then
    /// filtered by kind/agent/since and returned NEWEST FIRST, truncated to
    /// `limit`.
    pub fn search(
        &self,
        q: &str,
        kinds: &[String],
        agent: Option<&str>,
        since: Option<u64>,
        limit: usize,
    ) -> Vec<&EventDoc> {
        // Same hyphen story on the QUERY side: fold to spaces so partial
        // identities ("noble-otter") tokenize into matchable words.
        let q = q.replace(['-', '_'], " ");
        let q = q.as_str();

        let mut idxs: Vec<usize> = Vec::new();

        // Base: BM25 hits map back to doc indices via Section.line_number.
        if let Some(base) = &self.base {
            let hits = base.search(q, SearchVariant::Classic, &Bm25Params::default(), None);
            for h in hits {
                if let Some(sec) = base.sections.get(h.section_index) {
                    idxs.push(sec.line_number);
                }
            }
        }

        // Delta: every query token must appear (case-insensitive) in the text.
        let tokens: Vec<String> = lume::tokenize(q)
            .into_iter()
            .map(|t| String::from_utf8_lossy(&t.bytes).to_lowercase())
            .collect();
        if !tokens.is_empty() {
            for (i, d) in self.docs.iter().enumerate().skip(self.base_len) {
                let hay = d.text.to_lowercase();
                if tokens.iter().all(|t| hay.contains(t.as_str())) {
                    idxs.push(i);
                }
            }
        }

        idxs.sort_unstable();
        idxs.dedup();

        let mut out: Vec<&EventDoc> = idxs
            .into_iter()
            .filter_map(|i| self.docs.get(i))
            .filter(|d| kinds.is_empty() || kinds.contains(&d.kind))
            .filter(|d| agent.is_none_or(|a| d.agent.as_deref() == Some(a)))
            .filter(|d| since.is_none_or(|s| d.ts >= s))
            .collect();
        out.sort_by(|a, b| b.ts.cmp(&a.ts));
        out.truncate(limit);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(kind: &str, agent: &str, ts: u64, extra: &str) -> String {
        format!(
            r#"{{"kind":"{kind}","agent_id":"{agent}","ts":{ts},{extra}}}"#
        )
    }

    fn store_with(lines: &[String]) -> EventStore {
        let mut s = EventStore::new();
        for l in lines {
            s.ingest_line(l);
        }
        s
    }

    #[test]
    fn every_kind_is_indexed_and_agent_name_finds_all_of_them() {
        // Owner ruling: searching a container name must surface EVERYTHING it
        // does — metrics and heartbeats and file access included.
        let s = store_with(&[
            line("metric", "n8-noble-otter", 10, r#""cpu_pct":5"#),
            line("heartbeat", "n8-noble-otter", 11, r#""pid":1"#),
            line("fs", "n8-noble-otter", 12, r#""path":"/workspace/x.rs","kind_detail":"accessed""#),
        ]);
        assert_eq!(s.len(), 3);
        let hits = s.search("noble-otter", &[], None, None, 10);
        assert_eq!(hits.len(), 3);
        let kinds: Vec<_> = hits.iter().map(|d| d.kind.as_str()).collect();
        assert!(kinds.contains(&"metric") && kinds.contains(&"heartbeat") && kinds.contains(&"fs"));
    }

    #[test]
    fn delta_search_finds_paths_before_any_rebuild() {
        let s = store_with(&[
            line("fs", "a1", 10, r#""path":"/workspace/web/fleet.html","kind_detail":"modified""#),
            line("fs", "a2", 11, r#""path":"/workspace/src/main.rs","kind_detail":"accessed""#),
            line("status", "a1", 12, r#""status":"pulse","msg":"Transition to busy""#),
        ]);
        assert!(s.base.is_none(), "delta only — no rebuild yet");
        let hits = s.search("fleet.html", &[], None, None, 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].agent.as_deref(), Some("a1"));
        let hits = s.search("busy", &[], None, None, 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].kind, "status");
    }

    #[test]
    fn base_search_after_rebuild_and_newest_first_order() {
        let mut s = EventStore::new();
        s.rebuild_threshold = 3;
        for i in 0..5u64 {
            s.ingest_line(&line(
                "fs",
                "a1",
                100 + i,
                r#""path":"/workspace/report.md","kind_detail":"modified""#,
            ));
        }
        assert!(s.base.is_some(), "threshold crossed — base built");
        let hits = s.search("report.md", &[], None, None, 10);
        assert_eq!(hits.len(), 5);
        assert!(hits.windows(2).all(|w| w[0].ts >= w[1].ts), "newest first");
    }

    #[test]
    fn filters_kind_agent_since_and_limit() {
        let s = store_with(&[
            line("fs", "a1", 10, r#""path":"/w/x.rs","kind_detail":"accessed""#),
            line("fs", "a2", 20, r#""path":"/w/x.rs","kind_detail":"modified""#),
            line("status", "a2", 30, r#""msg":"x.rs compiled""#),
        ]);
        let hits = s.search("x.rs", &["fs".into()], None, None, 10);
        assert_eq!(hits.len(), 2);
        let hits = s.search("x.rs", &[], Some("a2"), None, 10);
        assert_eq!(hits.len(), 2);
        let hits = s.search("x.rs", &[], None, Some(15), 10);
        assert_eq!(hits.len(), 2);
        let hits = s.search("x.rs", &[], None, None, 1);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].ts, 30);
    }

    #[test]
    fn bulk_ingest_builds_base_once() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("events.jsonl");
        let mut body = String::new();
        for i in 0..50u64 {
            body.push_str(&line("fs", "a1", i, r#""path":"/w/deep/file.txt","kind_detail":"accessed""#));
            body.push('\n');
            body.push_str(&line("metric", "a1", i, r#""cpu_pct":1"#));
            body.push('\n');
        }
        std::fs::write(&p, body).unwrap();
        let mut s = EventStore::new();
        let added = s.ingest_file(&p).unwrap();
        assert_eq!(added, 100, "every kind indexed — metrics included");
        assert!(s.base.is_some());
        assert_eq!(s.base_len, 100);
        let hits = s.search("file.txt", &[], None, None, 100);
        assert_eq!(hits.len(), 50);
    }
}
