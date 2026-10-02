//! Plain stderr progress for setup stages that run before the build TUI.
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Announces a stage immediately and periodically reports that it is still running.
/// Dropping the guard stops and joins the reporter before the next stage starts.
pub struct SetupProgress {
    stop: Sender<()>,
    worker: Option<JoinHandle<()>>,
}

impl SetupProgress {
    pub fn new(stage: impl Into<String>) -> Self {
        use std::io::Write;
        Self::with_reporter(stage.into(), Duration::from_secs(10), |line| {
            let _ = writeln!(std::io::stderr(), "{line}");
        })
    }

    fn with_reporter(
        stage: String,
        interval: Duration,
        mut report: impl FnMut(&str) + Send + 'static,
    ) -> Self {
        report(&format!("[nemesis8] {stage}..."));
        let (stop, rx) = mpsc::channel();
        let start = Instant::now();
        let worker = std::thread::Builder::new()
            .name("setup-progress".into())
            .spawn(move || loop {
                match rx.recv_timeout(interval) {
                    Err(RecvTimeoutError::Timeout) => report(&format!(
                        "[nemesis8] Still running: {stage} ({}s elapsed).",
                        start.elapsed().as_secs()
                    )),
                    Ok(()) | Err(RecvTimeoutError::Disconnected) => break,
                }
            })
            .ok();
        Self { stop, worker }
    }
}

impl Drop for SetupProgress {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn announces_stage_then_heartbeats_until_dropped() {
        let (tx, rx) = mpsc::channel();
        let progress = SetupProgress::with_reporter(
            "Downloading build files".into(),
            Duration::from_millis(10),
            move |line| {
                let _ = tx.send(line.to_owned());
            },
        );
        assert_eq!(rx.recv().unwrap(), "[nemesis8] Downloading build files...");
        let heartbeat = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(heartbeat.starts_with("[nemesis8] Still running: Downloading build files ("));
        assert!(heartbeat.ends_with("s elapsed)."));
        drop(progress);
        // Drain any heartbeat emitted before drop; the worker must be gone.
        while rx.try_recv().is_ok() {}
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(2)),
            Err(RecvTimeoutError::Disconnected)
        );
    }

    #[test]
    fn dropping_does_not_wait_for_the_heartbeat_interval_or_claim_success() {
        let (tx, rx) = mpsc::channel();
        let progress = SetupProgress::with_reporter(
            "Extracting build files".into(),
            Duration::from_secs(60),
            move |line| {
                let _ = tx.send(line.to_owned());
            },
        );
        assert_eq!(rx.recv().unwrap(), "[nemesis8] Extracting build files...");
        let start = Instant::now();
        drop(progress);
        assert!(start.elapsed() < Duration::from_secs(2));
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(2)),
            Err(RecvTimeoutError::Disconnected)
        );
    }
}
