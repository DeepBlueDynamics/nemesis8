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

use nemisis8::monitor::{run_monitor, JsonlSink, EVENTS_FILE};

fn main() {
    let mut sink = match JsonlSink::new(EVENTS_FILE) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[nemesis8-monitor] could not open event sink: {e}");
            std::process::exit(1);
        }
    };

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
