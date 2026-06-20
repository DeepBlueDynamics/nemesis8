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

/// The recorded background-gateway PID, if any (public for the TUI).
pub fn running_pid() -> Option<u32> {
    read_pid()
}

/// True if something is accepting connections on the gateway port — a quick,
/// synchronous liveness check (real socket connect, not just a PID file) that's
/// safe to call from the control-room TUI. Localhost refusals return instantly;
/// the 200ms cap only bounds a pathological hang.
pub fn is_listening(port: u16) -> bool {
    use std::net::TcpStream;
    use std::net::ToSocketAddrs;
    format!("127.0.0.1:{port}")
        .to_socket_addrs()
        .ok()
        .and_then(|mut a| a.next())
        .map(|sa| TcpStream::connect_timeout(&sa, std::time::Duration::from_millis(200)).is_ok())
        .unwrap_or(false)
}

/// One-line gateway status for the TUI (no printing). Combines the live socket
/// check with the recorded PID so a stale pid file reads as stopped, not running.
pub fn status_line(port: u16) -> String {
    match (read_pid(), is_listening(port)) {
        (Some(p), true) => format!("running (pid {p}, :{port})"),
        (None, true) => format!("running (:{port}, no pid file)"),
        (Some(p), false) => format!("stopped (stale pid {p})"),
        (None, false) => "stopped".to_string(),
    }
}

/// Re-spawn this executable as a detached `serve` process (without
/// `--background`, so no recursion), redirect its stdout/stderr to the log
/// file, and record its PID. Returns the child PID; does NOT print, so callers
/// in a TUI (the control-room Gateway menu) aren't garbled — the CLI prints.
pub fn spawn_background(port: u16) -> Result<u32> {
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
        // CREATE_NO_WINDOW, not DETACHED_PROCESS: a console-subsystem exe spawned
        // with DETACHED_PROCESS still allocates its OWN console, which flashes a
        // cmd window on screen before vanishing. CREATE_NO_WINDOW runs it as a
        // windowless console app. CREATE_NEW_PROCESS_GROUP detaches it from the
        // parent's Ctrl+C group so it survives.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
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
    Ok(pid)
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

/// Stop the background gateway by its recorded PID. Returns Some(pid) if one was
/// recorded (the pid file is cleared either way), None if there was nothing to
/// stop. Silent + windowless (no console flash, no prints) so it's TUI-safe; the
/// CLI prints the outcome.
pub fn stop() -> Result<Option<u32>> {
    let Some(pid) = read_pid() else {
        return Ok(None);
    };

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/F", "/T"])
            .creation_flags(CREATE_NO_WINDOW)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    #[cfg(unix)]
    {
        let _ = std::process::Command::new("kill")
            .arg(pid.to_string())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }

    let _ = std::fs::remove_file(pid_path());
    Ok(Some(pid))
}
