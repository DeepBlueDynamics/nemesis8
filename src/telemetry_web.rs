//! Fleet telemetry web dashboard — issue #84.
//!
//! Serves a self-contained HTML dashboard (`web/fleet.html`) and a JSON blob
//! (`GET /fleet/data.json`) aggregating the live fleet view from
//! [`crate::telemetry::TelemetryState`].
//!
//! Intended to be nested into the gateway's router at integration time:
//!
//! ```ignore
//! let fleet = crate::telemetry_web::routes(app_state.telemetry.clone());
//! gateway_router = gateway_router.merge(fleet); // or .nest("/", fleet)
//! ```
//!
//! Two routes, both stateless:
//!   * `GET /fleet`          → the HTML page (inline, no external requests).
//!   * `GET /fleet/data.json`→ one JSON blob `{ fleet, net, events, health }`,
//!     shapes per `docs/plans/telemetry-mcp.md` (`fleet_status`,
//!     `agent_net`, `agent_events`).
//!
//! The fleet rows are derived from the telemetry index's newest `metric` event
//! per agent. `provider` / `workspace` / `state` / `uptime` come from the
//! metric event when present; until the gateway merge supplies container
//! labels (via `telemetry::fleet_rows(index, containers)`), they fall back to
//! `"running"` / `0` so the shape stays exactly the plan's `fleet_status` row.

use axum::{
    Router,
    extract::State,
    response::{Html, Json},
    routing::get,
};
use serde::Serialize;

/// The HTML page, compiled in via `include_str!` — one self-contained file,
/// inline CSS + vanilla JS, no external requests (offline box).
const FLEET_HTML: &str = include_str!("../web/fleet.html");

/// Build a standalone axum router serving the fleet dashboard.
///
/// `GET /`                → redirect to /fleet (a bare localhost:9801 in a
///                          browser should land somewhere, not 404).
/// `GET /fleet`           → the HTML dashboard.
/// `GET /fleet/data.json` → `{ fleet, net, events, health }` JSON blob.
pub fn routes(state: crate::telemetry::TelemetryState) -> Router {
    Router::new()
        .route(
            "/",
            get(|| async { axum::response::Redirect::to("/fleet") }),
        )
        .route("/fleet", get(fleet_html))
        .route("/fleet/data.json", get(fleet_data))
        .with_state(state)
}

async fn fleet_html() -> Html<&'static str> {
    Html(FLEET_HTML)
}

/// One fleet row as serialized to the dashboard. Mirrors the plan's
/// `fleet_status` tool row exactly.
#[derive(Serialize)]
struct FleetRowOut {
    agent_id: String,
    provider: String,
    workspace: String,
    state: String,
    uptime: u64,
    cpu_pct: f64,
    mem_used_kb: u64,
    net_rx_bps: u64,
    net_tx_bps: u64,
    last_ts: u64,
}

/// One event in the recent-events feed.
#[derive(Serialize)]
struct EventOut {
    kind: String,
    agent: Option<String>,
    ts: u64,
    summary: String,
}

/// The `/fleet/data.json` blob.
#[derive(Serialize)]
struct FleetData {
    fleet: Vec<FleetRowOut>,
    net: Vec<crate::telemetry::AgentNet>,
    events: Vec<EventOut>,
    health: crate::telemetry::TelemetryHealth,
}

/// Number of recent events to return per poll.
const EVENT_LIMIT: usize = 100;
/// Sparkline window — matches the plan's `agent_net(window=16)`.
const NET_WINDOW: usize = 16;

async fn fleet_data(
    State(state): State<crate::telemetry::TelemetryState>,
) -> Json<FleetData> {
    // Refresh the index from `events.jsonl` (if it changed), then read under a
    // single lock hold. refresh() takes the same mutex internally, so it MUST
    // run before we acquire the read lock.
    state.refresh();
    let index = state.index.lock().unwrap();

    let fleet = build_fleet(&index);
    let net = crate::telemetry::agent_net_stats(&index, NET_WINDOW);
    let events = build_events(&index, EVENT_LIMIT);
    // Health probe is cheap (reuses the same in-memory index) and lets the
    // dashboard surface "is aggregation even working" without a second call.
    // Always emitted — never null — so clients get a stable shape even when the
    // index is empty (health() degrades gracefully: indexed=0, tagged_ratio=1.0).
    let health = crate::telemetry::health(&index, &state.events_path);

    Json(FleetData { fleet, net, events, health })
}

/// Derive fleet rows from the index: newest `metric` event per tagged agent.
fn build_fleet(index: &crate::event_index::EventIndex) -> Vec<FleetRowOut> {
    use crate::event_index::EventQuery;
    // query() returns newest-first, so the first hit per agent_id is the newest.
    let metrics = index.query(&EventQuery {
        kinds: vec!["metric".to_string()],
        // usize::MAX, NOT 0: limit:0 falls back to DEFAULT_LIMIT (500) and would
        // silently cap the rollup, dropping agents during a busy window. Same
        // fix as telemetry.rs's rollups.
        limit: usize::MAX,
        ..Default::default()
    });

    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut rows: Vec<FleetRowOut> = Vec::new();
    for e in metrics {
        let Some(agent_id) = e.agent_id.as_ref() else {
            continue;
        };
        if !seen.insert(agent_id.clone()) {
            continue;
        }

        let provider = get_str(&e.raw, "provider");
        let mut state = get_str(&e.raw, "state");
        if state.is_empty() {
            // No container label in the event (yet) — the metric implies the
            // agent is alive, so we report `running`. The gateway merge can
            // override via fleet_rows(index, containers).
            state = "running".to_string();
        }

        rows.push(FleetRowOut {
            agent_id: agent_id.clone(),
            provider,
            workspace: get_str(&e.raw, "workspace"),
            state,
            uptime: get_u64(&e.raw, "uptime"),
            cpu_pct: get_f64(&e.raw, "cpu_pct"),
            mem_used_kb: get_u64(&e.raw, "mem_used_kb"),
            net_rx_bps: get_u64(&e.raw, "net_rx_bps"),
            net_tx_bps: get_u64(&e.raw, "net_tx_bps"),
            last_ts: e.ts,
        });
    }
    rows
}

/// Newest-first recent events, thinned to one-line summaries.
fn build_events(index: &crate::event_index::EventIndex, limit: usize) -> Vec<EventOut> {
    use crate::event_index::EventQuery;
    let events = index.query(&EventQuery {
        limit,
        ..Default::default()
    });
    events
        .into_iter()
        .map(|e| {
            let summary = e
                .raw
                .get("line")
                .and_then(|v| v.as_str())
                .or_else(|| e.raw.get("summary").and_then(|v| v.as_str()))
                .or_else(|| e.raw.get("message").and_then(|v| v.as_str()))
                .or_else(|| e.raw.get("path").and_then(|v| v.as_str()))
                .or_else(|| e.raw.get("op").and_then(|v| v.as_str()))
                .or_else(|| e.raw.get("error").and_then(|v| v.as_str()))
                .map(|s| truncate(s, 180))
                .unwrap_or_else(|| {
                    // Fall back to a compact, human-scanable form: the metric
                    // numbers for metric events, else the kind alone.
                    if e.kind == "metric" {
                        let cpu = get_f64(&e.raw, "cpu_pct");
                        let mem = get_u64(&e.raw, "mem_used_kb");
                        format!("cpu {:.0}% mem {:.0} MB", cpu, mem as f64 / 1024.0)
                    } else {
                        e.kind.clone()
                    }
                });
            EventOut {
                kind: e.kind.clone(),
                agent: e.agent_id.clone(),
                ts: e.ts,
                summary,
            }
        })
        .collect()
}

fn get_str(raw: &serde_json::Value, key: &str) -> String {
    raw.get(key)
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

fn get_u64(raw: &serde_json::Value, key: &str) -> u64 {
    raw.get(key)
        .and_then(|v| v.as_u64())
        .unwrap_or(0)
}

fn get_f64(raw: &serde_json::Value, key: &str) -> f64 {
    raw.get(key)
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0)
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let head: String = s.chars().take(n.saturating_sub(1)).collect();
        format!("{head}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event_index::EventIndex;
    use serde_json::json;

    #[test]
    fn fleet_from_index_newest_metric_per_agent() {
        let mut index = EventIndex::new(50);
        index.ingest_value(json!({
            "kind": "metric", "agent_id": "a1",
            "cpu_pct": 5.0, "mem_used_kb": 1024, "net_rx_bps": 10, "net_tx_bps": 5, "ts": 100
        }));
        index.ingest_value(json!({
            "kind": "metric", "agent_id": "a1",
            "cpu_pct": 42.0, "mem_used_kb": 4096, "net_rx_bps": 900, "net_tx_bps": 100, "ts": 200
        }));
        index.ingest_value(json!({
            "kind": "metric", "agent_id": "a2",
            "cpu_pct": 1.5, "mem_used_kb": 512, "net_rx_bps": 0, "net_tx_bps": 0, "ts": 150
        }));
        // untagged metric must be skipped
        index.ingest_value(json!({ "kind": "metric", "cpu_pct": 99.0, "ts": 999 }));

        let rows = build_fleet(&index);
        // stable order, newest-first per agent
        let ids: Vec<&str> = rows.iter().map(|r| r.agent_id.as_str()).collect();
        assert_eq!(ids, vec!["a1", "a2"]);

        let a1 = &rows[0];
        assert_eq!(a1.cpu_pct, 42.0); // newest wins
        assert_eq!(a1.mem_used_kb, 4096);
        assert_eq!(a1.net_rx_bps, 900);
        assert_eq!(a1.state, "running"); // default when absent
        assert_eq!(a1.provider, "");
        assert_eq!(a1.last_ts, 200);
    }

    /// Regression: build_fleet must not silently cap the rollup at
    /// DEFAULT_LIMIT (500). Ingest 600 distinct agents (>500) and confirm all
    /// 600 are returned. Previously `limit: 0` mapped to DEFAULT_LIMIT=500 and
    /// would have dropped the last 100.
    #[test]
    fn fleet_rollup_not_capped_at_default_limit() {
        let mut index = EventIndex::new(2000);
        for i in 0..600 {
            index.ingest_value(json!({
                "kind": "metric",
                "agent_id": format!("agent-{i}"),
                "cpu_pct": (i as f64) * 0.1,
                "mem_used_kb": 1024 * (i as u64),
                "ts": 1000 + i as u64,
            }));
        }
        let rows = build_fleet(&index);
        assert_eq!(rows.len(), 600, "all agents must roll up, no 500-cap");
        // spot-check boundary agents that the old 500-cap would have dropped
        assert!(rows.iter().any(|r| r.agent_id == "agent-499"));
        assert!(rows.iter().any(|r| r.agent_id == "agent-500"));
        assert!(rows.iter().any(|r| r.agent_id == "agent-599"));
    }

    #[test]
    fn events_summarize_and_truncate() {
        let mut index = EventIndex::new(50);
        index.ingest_value(json!({
            "kind": "logline", "agent_id": "a1", "ts": 10,
            "line": "short log line"
        }));
        index.ingest_value(json!({
            "kind": "metric", "agent_id": "a1", "ts": 11,
            "cpu_pct": 7.0, "mem_used_kb": 2048
        }));
        let long = "x".repeat(300);
        index.ingest_value(json!({
            "kind": "logline", "ts": 12, "line": long
        }));

        let ev = build_events(&index, 100);
        assert_eq!(ev.len(), 3);
        // newest-first
        assert_eq!(ev[0].kind, "logline");
        assert!(ev[0].agent.is_none());
        assert!(ev[0].summary.ends_with('…'));
        assert!(ev[0].summary.chars().count() <= 180);

        assert_eq!(ev[1].kind, "metric");
        assert_eq!(ev[1].agent.as_deref(), Some("a1"));
        assert!(ev[1].summary.contains("cpu"));
    }

    #[test]
    fn truncate_counts_graphemes_not_bytes_bound() {
        // ascii boundary
        assert_eq!(truncate("abcdef", 4), "abc…");
        // no truncation under limit
        assert_eq!(truncate("abc", 10), "abc");
    }
}
