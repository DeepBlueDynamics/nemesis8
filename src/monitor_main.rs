//! nemesis8-monitor: container telemetry daemon.
//!
//! Runs in the background inside every nemesis8 container, regardless of mode
//! (interactive / run / serve / shell). Watches the workspace filesystem for
//! activity, emits a heartbeat every 10s, writes structured events to
//! `/opt/nemesis8/.monitor/events.jsonl`.
//!
//! Spawned by `nemesis8-entry` early in the boot path (after the keyring is
//! ready, before the provider CLI launches). Tini reaps it when the parent
//! exits.

use std::path::Path;

use nemesis8::monitor::{run_monitor, EventSink, HttpSink, JsonlSink, TeeSink, EVENTS_FILE};

fn main() {
    // Always keep a durable local JSONL record. If this container was spawned
    // by a gateway (GATEWAY_URL + NEMESIS8_AGENT_ID present), ALSO push events
    // up to /agents/<id>/events so the control plane gets live telemetry.
    let jsonl = match JsonlSink::new(EVENTS_FILE) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[nemesis8-monitor] could not open event sink: {e}");
            std::process::exit(1);
        }
    };

    let mut sinks: Vec<Box<dyn EventSink>> = vec![Box::new(jsonl)];
    if let (Ok(gw), Ok(agent_id)) = (
        std::env::var("GATEWAY_URL"),
        std::env::var("NEMESIS8_AGENT_ID"),
    ) {
        let url = format!("{}/agents/{}/events", gw.trim_end_matches('/'), agent_id);
        let token = std::env::var("NEMESIS8_AUTH_TOKEN").ok();
        eprintln!("[nemesis8-monitor] pushing telemetry to {url}");
        sinks.push(Box::new(HttpSink::new(url, token)));
    }
    let mut sink = TeeSink::new(sinks);

    // Workspace is the only thing worth watching by default. /opt/nemesis8
    // would generate a torrent of self-noise from agy's brain dir.
    let workspace = std::env::var("NEMESIS8_WORKSPACE")
        .unwrap_or_else(|_| "/workspace".to_string());
    let watch_dirs: Vec<&Path> = vec![Path::new(&workspace)];

    if let Err(e) = run_monitor(&watch_dirs, 10, &mut sink) {
        eprintln!("[nemesis8-monitor] exited with error: {e}");
        std::process::exit(1);
    }
}
