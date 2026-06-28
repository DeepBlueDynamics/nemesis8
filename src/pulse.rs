use std::time::{Duration, Instant};
use crate::monitor::{http_post_json, now_ts, EventSink, MonitorEvent};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PulseState {
    Busy,
    Idle,
}

pub struct PulseEmitter {
    url: String,
    token: Option<String>,
    state: PulseState,
    consecutive_idle_ticks: u32,
    last_post_time: Option<Instant>,
}

impl PulseEmitter {
    pub fn new() -> Self {
        let url = std::env::var("HYPERIA_URL")
            .unwrap_or_else(|_| "http://host.docker.internal:9800".to_string());
        let token = std::env::var("HYPERIA_AGENT_TOKEN").ok();
        
        Self {
            url,
            token,
            state: PulseState::Idle,
            consecutive_idle_ticks: 0,
            last_post_time: None,
        }
    }

    pub fn tick(&mut self, is_busy: bool, sink: &mut dyn EventSink) {
        let token = match &self.token {
            Some(t) if !t.is_empty() => t,
            _ => {
                // If token missing, no-op cleanly.
                return;
            }
        };

        let now = Instant::now();

        if is_busy {
            self.consecutive_idle_ticks = 0;
            
            let should_post = match self.state {
                PulseState::Idle => {
                    self.state = PulseState::Busy;
                    let msg = format!("Transition to busy. URL: {}", self.url);
                    let _ = sink.write_event(&MonitorEvent::Status {
                        ts: now_ts(),
                        status: "pulse".to_string(),
                        msg,
                    });
                    true
                }
                PulseState::Busy => {
                    // keepalive every ~5s
                    match self.last_post_time {
                        Some(last) => now.duration_since(last) >= Duration::from_secs(5),
                        None => true,
                    }
                }
            };

            if should_post {
                let body = r#"{"state":"busy","ttl_secs":10}"#;
                let post_url = format!("{}/api/pulse/liveness", self.url.trim_end_matches('/'));
                
                let _ = sink.write_event(&MonitorEvent::Status {
                    ts: now_ts(),
                    status: "pulse_post".to_string(),
                    msg: format!("POSTing busy to {}", post_url),
                });

                if let Err(e) = http_post_json(&post_url, body, Some(token)) {
                    let _ = sink.write_event(&MonitorEvent::Status {
                        ts: now_ts(),
                        status: "pulse_error".to_string(),
                        msg: format!("Failed to POST liveness: {}", e),
                    });
                } else {
                    self.last_post_time = Some(now);
                }
            }
        } else {
            self.consecutive_idle_ticks += 1;
            
            // stayed idle for N consecutive ticks (grace ~6s, tick is 2s => 3 ticks)
            if self.consecutive_idle_ticks >= 3 {
                if self.state == PulseState::Busy {
                    self.state = PulseState::Idle;
                    let msg = format!("Transition to idle. URL: {}", self.url);
                    let _ = sink.write_event(&MonitorEvent::Status {
                        ts: now_ts(),
                        status: "pulse".to_string(),
                        msg,
                    });

                    let body = r#"{"state":"idle"}"#;
                    let post_url = format!("{}/api/pulse/liveness", self.url.trim_end_matches('/'));
                    
                    let _ = sink.write_event(&MonitorEvent::Status {
                        ts: now_ts(),
                        status: "pulse_post".to_string(),
                        msg: format!("POSTing idle to {}", post_url),
                    });

                    if let Err(e) = http_post_json(&post_url, body, Some(token)) {
                        let _ = sink.write_event(&MonitorEvent::Status {
                            ts: now_ts(),
                            status: "pulse_error".to_string(),
                            msg: format!("Failed to POST idle liveness: {}", e),
                        });
                    }
                }
            }
        }
    }
}
