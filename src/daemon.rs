//! Daemon-mode helpers for `n8 serve`.
//!
//! `n8 serve` runs the gateway in the foreground by default. These helpers add
//! a first-class background mode so the control plane can run without a
//! babysitting terminal:
//!   - `spawn_background` — re-spawn this exe as a detached `serve` process,
//!     redirect its output to a log file, record its PID.
//!   - `status` — report whether the gateway is up (PID file + /health probe).
//!   - `stop` — terminate the recorded PID.
//!
//! State lives under ~/.nemesis8/home/ (same dir as the trigger store):
//!   gateway.pid, gateway.log

use anyhow::{Context, Result};
use std::path::PathBuf;

fn service_dir() -> PathBuf {
    crate::paths::data_home()
}

pub fn pid_path() -> PathBuf {
    service_dir().join("gateway.pid")
}

pub fn log_path() -> PathBuf {
    service_dir().join("gateway.log")
}

fn read_pid() -> Option<u32> {
    std::fs::read_to_string(pid_path())
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
}

/// Re-spawn this executable as a detached `serve` process (without
/// `--background`, so no recursion), redirect its stdout/stderr to the log
/// file, and record its PID. The parent returns immediately.
pub fn spawn_background(port: u16) -> Result<()> {
    let dir = service_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;

    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path())
        .with_context(|| format!("opening {}", log_path().display()))?;
    let log_err = log.try_clone()?;

    let exe = std::env::current_exe().context("resolving current exe")?;

    let mut cmd = std::process::Command::new(exe);
    cmd.arg("serve")
        .arg("--port")
        .arg(port.to_string())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(log))
        .stderr(std::process::Stdio::from(log_err));

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // New process group so the daemon survives the parent shell and isn't
        // killed by terminal signals.
        cmd.process_group(0);
    }

    let child = cmd.spawn().context("spawning background gateway")?;
    let pid = child.id();
    std::fs::write(pid_path(), pid.to_string())
        .with_context(|| format!("writing {}", pid_path().display()))?;

    println!("nemesis8 gateway started in background (pid {pid}, port {port})");
    println!("  logs:   {}", log_path().display());
    println!("  status: n8 serve --status");
    println!("  stop:   n8 serve --stop");
    Ok(())
}

/// Report whether the gateway is running, using the PID file plus a /health
/// probe so we catch stale PID files (process gone but file left behind).
pub async fn status(port: u16) -> Result<()> {
    let pid = read_pid();
    let url = format!("http://127.0.0.1:{port}/health");
    let healthy = match reqwest::Client::new()
        .get(&url)
        .timeout(std::time::Duration::from_secs(2))
        .send()
        .await
    {
        Ok(r) => r.status().is_success(),
        Err(_) => false,
    };

    match (pid, healthy) {
        (Some(p), true) => println!("running  (pid {p}, port {port})"),
        (Some(p), false) => println!(
            "not responding (pid {p} recorded but /health failed on port {port}) — try `n8 serve --stop` then `--background`"
        ),
        (None, true) => println!("running (port {port}, no pid file — started without --background)"),
        (None, false) => println!("not running"),
    }
    Ok(())
}

/// Stop the background gateway by its recorded PID.
pub fn stop() -> Result<()> {
    let Some(pid) = read_pid() else {
        println!("no pid file at {}; nothing to stop", pid_path().display());
        return Ok(());
    };

    #[cfg(windows)]
    let ok = std::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/F", "/T"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    #[cfg(unix)]
    let ok = std::process::Command::new("kill")
        .arg(pid.to_string())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    let _ = std::fs::remove_file(pid_path());
    if ok {
        println!("stopped gateway (pid {pid})");
    } else {
        println!("pid {pid} was already gone; removed stale pid file");
    }
    Ok(())
}
