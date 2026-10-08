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

use nemesis8::monitor::{run_monitor, JsonlSink, EVENTS_FILE};

fn main() {
    // One durable local JSONL record. `/opt/nemesis8` is a bind mount of the
    // host's data home, so this file IS the gateway's telemetry source
    // (`GET /monitor/events`, the fleet console, the search store) — there is
    // no per-event HTTP push. The push that used to sit beside this sink
    // targeted `POST /agents/<id>/events`, a route the gateway never
    // registered: every event drew a 404 and, before the pooled client, a
    // fresh TCP connection that helped exhaust the host's ephemeral ports.
    //
    // Every container mounts the same host data home, so a single shared
    // `events.jsonl` took concurrent cross-container appends — which the
    // bind-mount FUSE layer does NOT serialise — and interleaved them into
    // corrupt "glued" lines the gateway's line tail then dropped (~2/3 loss).
    // The launcher gives each container its own `NEMESIS8_EVENTS_FILE`
    // (`events.<agent_id>.jsonl`); honour it here and fall back to the shared
    // path only when it's unset (older launchers, bare `n8 run`).
    let events_file =
        std::env::var("NEMESIS8_EVENTS_FILE").unwrap_or_else(|_| EVENTS_FILE.to_string());
    let mut sink = match JsonlSink::new(events_file.as_str()) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[nemesis8-monitor] could not open event sink: {e}");
            std::process::exit(1);
        }
    };
    if let Ok(gw) = std::env::var("GATEWAY_URL") {
        eprintln!("[nemesis8-monitor] gateway {gw} reads telemetry from {events_file}");
    }

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
