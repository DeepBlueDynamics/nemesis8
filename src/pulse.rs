use std::time::{Duration, Instant};
use crate::monitor::{http_post_json, now_ts, EventSink, MonitorEvent};

/// Re-post liveness at least this often, in BOTH states, so Hyperia keeps
/// seeing the pane as a live agent. Must stay under the smaller `ttl_secs`
/// below (10 s busy) with margin.
const KEEPALIVE: Duration = Duration::from_secs(5);
/// Idle for this many ticks (tick = 2 s ⇒ ~6 s) before we declare idle, so a
/// brief gap between the agent's outputs doesn't flap the state.
const IDLE_GRACE_TICKS: u32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PulseState {
    Busy,
    Idle,
}

impl PulseState {
    /// The liveness body for this state. It carries `agent:"n8"` so Hyperia can
    /// recognise the pane as an agent even when shell integration was cleared
    /// (e.g. after the PC sleeps and the sidecar socket reconnects), and a
    /// `ttl_secs` so a missed post expires instead of sticking. Idle's ttl is a
    /// touch longer than its keepalive so one dropped post doesn't flap.
    fn liveness_body(self) -> &'static str {
        match self {
            PulseState::Busy => r#"{"state":"busy","agent":"n8","ttl_secs":10}"#,
            PulseState::Idle => r#"{"state":"idle","agent":"n8","ttl_secs":15}"#,
        }
    }
    fn label(self) -> &'static str {
        match self {
            PulseState::Busy => "busy",
            PulseState::Idle => "idle",
        }
    }
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

    /// Pure decision (no I/O): advance state/counters for this tick and return
    /// `Some(state)` when a liveness POST is due now — on a state transition or
    /// on the ~5 s keepalive. Both busy AND idle post on the keepalive; idle
    /// used to post once on the busy→idle transition and then go silent, which
    /// made an idle agent invisible to Hyperia. `last_post_time` is NOT updated
    /// here — the caller sets it only after a POST actually succeeds, so a
    /// failed post retries on the next tick.
    fn decide(&mut self, now: Instant, is_busy: bool) -> Option<PulseState> {
        let keepalive_due = self
            .last_post_time
            .map_or(true, |last| now.duration_since(last) >= KEEPALIVE);

        if is_busy {
            self.consecutive_idle_ticks = 0;
            let transition = self.state == PulseState::Idle;
            self.state = PulseState::Busy;
            (transition || keepalive_due).then_some(PulseState::Busy)
        } else {
            self.consecutive_idle_ticks += 1;
            if self.consecutive_idle_ticks < IDLE_GRACE_TICKS {
                return None;
            }
            let transition = self.state == PulseState::Busy;
            self.state = PulseState::Idle;
            (transition || keepalive_due).then_some(PulseState::Idle)
        }
    }

    pub fn tick(&mut self, is_busy: bool, sink: &mut dyn EventSink) {
        let token = match &self.token {
            Some(t) if !t.is_empty() => t.clone(),
            _ => return, // no token → no-op cleanly, no POST
        };

        let now = Instant::now();
        let prev = self.state;
        let Some(state) = self.decide(now, is_busy) else {
            return;
        };

        if state != prev {
            let _ = sink.write_event(&MonitorEvent::Status {
                ts: now_ts(),
                status: "pulse".to_string(),
                msg: format!("Transition to {}. URL: {}", state.label(), self.url),
            });
        }

        let post_url = format!("{}/api/pulse/liveness", self.url.trim_end_matches('/'));
        let _ = sink.write_event(&MonitorEvent::Status {
            ts: now_ts(),
            status: "pulse_post".to_string(),
            msg: format!("POSTing {} to {}", state.label(), post_url),
        });

        match http_post_json(&post_url, state.liveness_body(), Some(&token)) {
            Ok(_) => self.last_post_time = Some(now),
            Err(e) => {
                let _ = sink.write_event(&MonitorEvent::Status {
                    ts: now_ts(),
                    status: "pulse_error".to_string(),
                    msg: format!("Failed to POST liveness: {}", e),
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn emitter() -> PulseEmitter {
        PulseEmitter {
            url: "http://127.0.0.1:9800".to_string(),
            token: Some("hyp_test".to_string()),
            state: PulseState::Idle,
            consecutive_idle_ticks: 0,
            last_post_time: None,
        }
    }

    #[test]
    fn liveness_body_carries_agent_and_ttl() {
        let b = PulseState::Busy.liveness_body();
        assert!(b.contains(r#""state":"busy""#), "{b}");
        assert!(b.contains(r#""agent":"n8""#), "busy body needs agent: {b}");
        assert!(b.contains(r#""ttl_secs":10"#), "{b}");
        let i = PulseState::Idle.liveness_body();
        assert!(i.contains(r#""state":"idle""#), "{i}");
        assert!(i.contains(r#""agent":"n8""#), "idle body needs agent: {i}");
        assert!(i.contains(r#""ttl_secs":15"#), "idle keepalive ttl: {i}");
    }

    // `decide` is pure, so the cadence is testable with synthetic time and no
    // network. The caller sets last_post_time only on a successful post, so the
    // test does the same (post_ok) to mimic tick.
    #[test]
    fn idle_keeps_posting_on_the_keepalive_cadence() {
        let mut e = emitter();
        let t0 = Instant::now();
        let post_ok = |e: &mut PulseEmitter, t: Instant| e.last_post_time = Some(t);

        // Busy transition posts immediately, then every ~5s.
        assert_eq!(e.decide(t0, true), Some(PulseState::Busy));
        post_ok(&mut e, t0);
        assert_eq!(e.decide(t0 + Duration::from_secs(2), true), None, "keepalive not due yet");
        assert_eq!(e.decide(t0 + Duration::from_secs(5), true), Some(PulseState::Busy), "busy keepalive");
        post_ok(&mut e, t0 + Duration::from_secs(5));

        // Go idle: grace is 3 ticks, so the first two idle ticks post nothing.
        assert_eq!(e.decide(t0 + Duration::from_secs(7), false), None, "grace tick 1");
        assert_eq!(e.decide(t0 + Duration::from_secs(9), false), None, "grace tick 2");
        // Third idle tick crosses the grace → idle transition posts.
        assert_eq!(e.decide(t0 + Duration::from_secs(11), false), Some(PulseState::Idle), "idle transition");
        post_ok(&mut e, t0 + Duration::from_secs(11));

        // Idle is NOT a one-shot: it keeps posting on the keepalive cadence.
        assert_eq!(e.decide(t0 + Duration::from_secs(13), false), None, "idle keepalive not due");
        assert_eq!(e.decide(t0 + Duration::from_secs(16), false), Some(PulseState::Idle), "idle keepalive fires");
    }

    #[test]
    fn fresh_start_idle_becomes_visible() {
        // An agent that starts idle (never busy) must still announce itself.
        let mut e = emitter();
        let t0 = Instant::now();
        assert_eq!(e.decide(t0, false), None); // grace
        assert_eq!(e.decide(t0 + Duration::from_secs(2), false), None);
        assert_eq!(e.decide(t0 + Duration::from_secs(4), false), Some(PulseState::Idle), "idle agent posts after grace");
    }

    // Collects events so we can assert tick made no POST without a token.
    struct RecordingSink(Vec<String>);
    impl EventSink for RecordingSink {
        fn write_event(&mut self, event: &MonitorEvent) -> anyhow::Result<()> {
            if let MonitorEvent::Status { status, .. } = event {
                self.0.push(status.clone());
            }
            Ok(())
        }
    }

    #[test]
    fn no_token_means_no_post() {
        let mut e = PulseEmitter {
            url: "http://127.0.0.1:9800".to_string(),
            token: None,
            state: PulseState::Idle,
            consecutive_idle_ticks: 0,
            last_post_time: None,
        };
        let mut sink = RecordingSink(Vec::new());
        e.tick(true, &mut sink);
        assert!(sink.0.is_empty(), "no token: tick must not post or log, got {:?}", sink.0);
        assert!(e.last_post_time.is_none());
    }
}
