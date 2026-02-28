//! pokeball-worker: runs inside the sealed container.
//! Watches /comms/inbox for messages, executes tool calls, writes results to /comms/outbox.

use std::path::Path;

fn main() {
    eprintln!("pokeball-worker starting...");

    let inbox = Path::new("/comms/inbox");
    let outbox = Path::new("/comms/outbox");
    let status_dir = Path::new("/comms/status");

    // Create dirs if needed
    let _ = std::fs::create_dir_all(inbox);
    let _ = std::fs::create_dir_all(outbox);
    let _ = std::fs::create_dir_all(status_dir);

    // Track seen files
    let mut seen = std::collections::HashSet::new();

    // Record existing files
    if let Ok(entries) = std::fs::read_dir(inbox) {
        for entry in entries.flatten() {
            if let Some(name) = entry.file_name().to_str() {
                seen.insert(name.to_string());
            }
        }
    }

    eprintln!("pokeball-worker: watching /comms/inbox for messages");

    let mut seq_counter: u32 = 100; // worker starts at 100 to not collide with broker

    loop {
        std::thread::sleep(std::time::Duration::from_millis(100));

        // Write heartbeat every ~5 seconds (every 50 iterations)
        if seq_counter % 50 == 0 {
            write_heartbeat(status_dir);
        }

        let entries = match std::fs::read_dir(inbox) {
            Ok(e) => e,
            Err(_) => continue,
        };

        for entry in entries.flatten() {
            let name = match entry.file_name().to_str() {
                Some(n) => n.to_string(),
                None => continue,
            };

            if name.starts_with('.') || !name.ends_with(".json") {
                continue;
            }

            if seen.contains(&name) {
                continue;
            }
            seen.insert(name.clone());

            let path = entry.path();
            let content = match std::fs::read_to_string(&path) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("pokeball-worker: error reading {}: {e}", path.display());
                    continue;
                }
            };

            let msg: nemisis8::pokeball::protocol::Message = match serde_json::from_str(&content) {
                Ok(m) => m,
                Err(e) => {
                    eprintln!("pokeball-worker: malformed message {}: {e}", name);
                    continue;
                }
            };

            match msg {
                nemisis8::pokeball::protocol::Message::Turn { tool_calls, id, .. } => {
                    eprintln!(
                        "pokeball-worker: received turn {} with {} tool calls",
                        id,
                        tool_calls.len()
                    );

                    let mut results = Vec::new();
                    for tc in &tool_calls {
                        eprintln!("pokeball-worker: executing {} ({})", tc.name, tc.id);
                        let output =
                            nemisis8::pokeball::tools::execute_tool(&tc.name, &tc.input, &tc.id);
                        if output.success {
                            eprintln!(
                                "pokeball-worker: {} succeeded ({} bytes output)",
                                tc.name,
                                output.output.len()
                            );
                        } else {
                            eprintln!(
                                "pokeball-worker: {} failed: {}",
                                tc.name,
                                output.error.as_deref().unwrap_or("unknown")
                            );
                        }
                        results.push(output);
                    }

                    seq_counter += 1;
                    let response = nemisis8::pokeball::protocol::Message::TurnComplete {
                        seq: seq_counter,
                        id: uuid_v4(),
                        results,
                    };

                    if let Err(e) =
                        nemisis8::pokeball::protocol::send_message(outbox, &response)
                    {
                        eprintln!("pokeball-worker: error writing response: {e}");
                    }
                }
                nemisis8::pokeball::protocol::Message::Shutdown { .. } => {
                    eprintln!("pokeball-worker: received shutdown, exiting");
                    return;
                }
                other => {
                    eprintln!("pokeball-worker: ignoring message type: {:?}", other);
                }
            }
        }
    }
}

fn write_heartbeat(status_dir: &Path) {
    let ts = chrono::Utc::now().to_rfc3339();
    let heartbeat = serde_json::json!({
        "type": "heartbeat",
        "timestamp": ts,
        "pid": std::process::id(),
    });
    let path = status_dir.join("heartbeat.json");
    let _ = std::fs::write(path, serde_json::to_string(&heartbeat).unwrap_or_default());
}

fn uuid_v4() -> String {
    uuid::Uuid::new_v4().to_string()
}
