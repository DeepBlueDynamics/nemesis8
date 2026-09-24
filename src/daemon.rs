//! Daemon-mode helpers for `n8 serve`.
//!
//! `n8 serve` runs the gateway in the foreground by default. These helpers add
//! a first-class background mode so the control plane can run without a
//! babysitting terminal:
//!   - `spawn_background` — re-spawn this exe as a detached `serve` process,
//!     redirect its output to a log file, record its PID.
//!   - `status` — report whether the gateway is up (PID file + /health probe).
//!   - `stop` — terminate the recorded PID, or a foreground gateway found by
//!     the port it listens on.
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

/// What `stop` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopOutcome {
    /// A gateway was stopped. `recorded` is true for the `--background` daemon
    /// (pid file), false for a foreground `n8 serve` found by the port it
    /// listens on.
    Stopped { pid: u32, recorded: bool },
    /// No pid recorded and nothing listening on the port.
    NotRunning,
    /// Something that is not an n8 binary owns the port. Left alone.
    ForeignListener { pid: u32, name: String },
    /// Nothing on this port, but the recorded daemon is alive and serving
    /// other ports. Left alone — pass its port to stop it.
    RecordedElsewhere { pid: u32, ports: Vec<u16> },
}

/// Stop the gateway. Prefers the recorded `--background` PID; without one, a
/// gateway started in the foreground (`n8 serve` in a pane) has no pid file,
/// so we find the process listening on `port` and stop exactly that PID if
/// it is an n8/nemesis8 binary. Until 0.25.4 that case reported "nothing to
/// stop" and the gateway kept running. Silent + windowless (no console
/// flash, no prints) so it's TUI-safe; the callers print the outcome.
pub fn stop(port: u16) -> Result<StopOutcome> {
    // A recorded PID counts only while it is still one of our binaries. A
    // stale pid file (daemon long gone, PID possibly reused by something
    // else) used to get `taskkill`ed blindly; now it is just cleared.
    let recorded_raw = read_pid();
    let recorded = recorded_raw.filter(|&p| is_n8_process(&process_name(p).unwrap_or_default()));
    if recorded_raw.is_some() && recorded.is_none() {
        let _ = std::fs::remove_file(pid_path());
    }
    let listener = listener_pid(port).map(|p| (p, process_name(p).unwrap_or_default()));
    let recorded_ports = recorded.map(listening_ports_of).unwrap_or_default();

    match plan_stop(recorded, &recorded_ports, listener, std::process::id()) {
        StopPlan::Kill { pid, recorded } => {
            kill_pid(pid);
            if recorded {
                let _ = std::fs::remove_file(pid_path());
            }
            Ok(StopOutcome::Stopped { pid, recorded })
        }
        StopPlan::Foreign { pid, name } => Ok(StopOutcome::ForeignListener { pid, name }),
        StopPlan::Elsewhere { pid, ports } => Ok(StopOutcome::RecordedElsewhere { pid, ports }),
        StopPlan::Nothing => Ok(StopOutcome::NotRunning),
    }
}

#[derive(Debug, PartialEq, Eq)]
enum StopPlan {
    Kill { pid: u32, recorded: bool },
    Foreign { pid: u32, name: String },
    Elsewhere { pid: u32, ports: Vec<u16> },
    Nothing,
}

/// Decide what `stop(port)` kills. The gateway actually listening on `port`
/// wins, and a daemon serving some OTHER port is never touched:
/// `--stop --port 9811` took down the recorded daemon on 9801 twice during
/// testing, once through the blind pid-file path and once through a
/// "recorded but not listening here, must be hung" rule. Now the recorded
/// daemon is stopped only when it IS the listener on `port`, or when it
/// listens nowhere at all (hung or still starting). `recorded` has already
/// been verified to be one of our binaries; `recorded_ports` are the ports
/// it currently listens on; `listener` carries the executable name of
/// whatever holds `port`.
fn plan_stop(
    recorded: Option<u32>,
    recorded_ports: &[u16],
    listener: Option<(u32, String)>,
    me: u32,
) -> StopPlan {
    match (recorded, listener) {
        (Some(r), Some((l, _))) if r == l => StopPlan::Kill { pid: r, recorded: true },
        (_, Some((l, name))) => {
            if l == me || !is_n8_process(&name) {
                StopPlan::Foreign { pid: l, name }
            } else {
                StopPlan::Kill { pid: l, recorded: false }
            }
        }
        (Some(r), None) if recorded_ports.is_empty() => StopPlan::Kill { pid: r, recorded: true },
        (Some(r), None) => StopPlan::Elsewhere { pid: r, ports: recorded_ports.to_vec() },
        (None, None) => StopPlan::Nothing,
    }
}

/// TCP ports `pid` is listening on (empty if none or unknown).
fn listening_ports_of(pid: u32) -> Vec<u16> {
    #[cfg(windows)]
    {
        quiet_output("netstat", &["-ano", "-p", "tcp"])
            .map(|o| parse_netstat_ports_of(&o, pid))
            .unwrap_or_default()
    }
    #[cfg(unix)]
    {
        quiet_output("ss", &["-ltnpH"])
            .map(|o| parse_ss_ports_of(&o, pid))
            .unwrap_or_default()
    }
}

/// `netstat -ano -p tcp` → local ports of the LISTENING rows owned by `pid`.
fn parse_netstat_ports_of(out: &str, pid: u32) -> Vec<u16> {
    let mut ports: Vec<u16> = out
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            if f.len() < 5
                || !f[0].eq_ignore_ascii_case("tcp")
                || !f[3].eq_ignore_ascii_case("listening")
                || f[4].parse::<u32>().ok()? != pid
            {
                return None;
            }
            f[1].rsplit(':').next()?.parse::<u16>().ok()
        })
        .collect();
    ports.sort_unstable();
    ports.dedup();
    ports
}

/// `ss -ltnpH` → local ports of the rows whose `users:(...)` names `pid`.
#[cfg_attr(windows, allow(dead_code))]
fn parse_ss_ports_of(out: &str, pid: u32) -> Vec<u16> {
    let tag = format!("pid={pid},");
    let mut ports: Vec<u16> = out
        .lines()
        .filter(|l| l.contains(&tag))
        .filter_map(|l| {
            // LISTEN 0 1024 0.0.0.0:9801 0.0.0.0:* users:(("n8",pid=7,fd=9))
            let local = l.split_whitespace().nth(3)?;
            local.rsplit(':').next()?.parse::<u16>().ok()
        })
        .collect();
    ports.sort_unstable();
    ports.dedup();
    ports
}

/// Terminate exactly this PID (and, on Windows, its process tree — the
/// gateway's `docker exec` children). Best-effort, silent.
fn kill_pid(pid: u32) {
    #[cfg(windows)]
    {
        let _ = quiet_output("taskkill", &["/PID", &pid.to_string(), "/F", "/T"]);
    }
    #[cfg(unix)]
    {
        let _ = quiet_output("kill", &[&pid.to_string()]);
    }
}

/// Run a helper command with no console window and return its stdout.
fn quiet_output(program: &str, args: &[&str]) -> Option<String> {
    let mut cmd = std::process::Command::new(program);
    cmd.args(args)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let out = cmd.output().ok()?;
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// PID of the process listening on TCP `port`, if any.
fn listener_pid(port: u16) -> Option<u32> {
    #[cfg(windows)]
    {
        parse_netstat_listener(&quiet_output("netstat", &["-ano", "-p", "tcp"])?, port)
    }
    #[cfg(unix)]
    {
        let spec = format!("-iTCP:{port}");
        if let Some(out) = quiet_output("lsof", &["-nP", "-t", &spec, "-sTCP:LISTEN"]) {
            if let Some(pid) = out.lines().find_map(|l| l.trim().parse::<u32>().ok()) {
                return Some(pid);
            }
        }
        let filter = format!("sport = :{port}");
        parse_ss_listener(&quiet_output("ss", &["-ltnpH", &filter])?)
    }
}

/// Executable name of `pid` (basename, e.g. `n8.exe`), if the process exists.
fn process_name(pid: u32) -> Option<String> {
    #[cfg(windows)]
    {
        let filter = format!("PID eq {pid}");
        parse_tasklist_name(&quiet_output("tasklist", &["/FI", &filter, "/FO", "CSV", "/NH"])?)
    }
    #[cfg(unix)]
    {
        if let Ok(comm) = std::fs::read_to_string(format!("/proc/{pid}/comm")) {
            let c = comm.trim();
            if !c.is_empty() {
                return Some(c.to_string());
            }
        }
        let out = quiet_output("ps", &["-p", &pid.to_string(), "-o", "comm="])?;
        let c = out.trim();
        (!c.is_empty()).then(|| c.rsplit('/').next().unwrap_or(c).to_string())
    }
}

/// `netstat -ano -p tcp` → the PID in the LISTENING row for `port`.
///   TCP    0.0.0.0:9801    0.0.0.0:0    LISTENING    12688
fn parse_netstat_listener(out: &str, port: u16) -> Option<u32> {
    let suffix = format!(":{port}");
    out.lines().find_map(|l| {
        let f: Vec<&str> = l.split_whitespace().collect();
        if f.len() < 5 || !f[0].eq_ignore_ascii_case("tcp") {
            return None;
        }
        if !f[1].ends_with(&suffix) || !f[3].eq_ignore_ascii_case("listening") {
            return None;
        }
        f[4].parse::<u32>().ok()
    })
}

/// `ss -ltnpH 'sport = :9801'` → the `pid=NNN` inside `users:(("n8",pid=12688,fd=9))`.
#[cfg_attr(windows, allow(dead_code))]
fn parse_ss_listener(out: &str) -> Option<u32> {
    out.split("pid=")
        .skip(1)
        .find_map(|rest| rest.chars().take_while(|c| c.is_ascii_digit()).collect::<String>().parse::<u32>().ok())
}

/// `tasklist /FO CSV /NH` → the image name in the first quoted field.
///   "n8.exe","12688","Console","1","123,456 K"
fn parse_tasklist_name(out: &str) -> Option<String> {
    let line = out.lines().map(str::trim).find(|l| l.starts_with('"'))?;
    let name = line.trim_start_matches('"').split('"').next()?;
    (!name.is_empty()).then(|| name.to_string())
}

/// Is this executable name one of ours? (`n8`, `n8.exe`, `nemesis8`, `nemesis8.exe`,
/// with or without a directory.)
fn is_n8_process(name: &str) -> bool {
    let base = name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(name)
        .to_ascii_lowercase();
    let stem = base.strip_suffix(".exe").unwrap_or(&base);
    stem == "n8" || stem == "nemesis8"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn netstat_row_for_the_port_only_when_listening() {
        let out = "\n  Proto  Local Address          Foreign Address        State           PID\n\
  TCP    0.0.0.0:9800           0.0.0.0:0              LISTENING       4242\n\
  TCP    127.0.0.1:9801         127.0.0.1:51000        ESTABLISHED     12688\n\
  TCP    0.0.0.0:9801           0.0.0.0:0              LISTENING       12688\n\
  TCP    [::]:9803              [::]:0                 LISTENING       12688\n\
  UDP    0.0.0.0:9801           *:*                                    999\n";
        assert_eq!(parse_netstat_listener(out, 9801), Some(12688));
        assert_eq!(parse_netstat_listener(out, 9803), Some(12688));
        assert_eq!(parse_netstat_listener(out, 9800), Some(4242));
        assert_eq!(parse_netstat_listener(out, 9802), None);
        // ":980" must not match ":9801" by prefix
        assert_eq!(parse_netstat_listener(out, 980), None);
    }

    #[test]
    fn ss_and_tasklist_parsers() {
        assert_eq!(
            parse_ss_listener("LISTEN 0 1024 0.0.0.0:9801 0.0.0.0:* users:((\"n8\",pid=31337,fd=9))\n"),
            Some(31337)
        );
        assert_eq!(parse_ss_listener(""), None);
        assert_eq!(
            parse_tasklist_name("\"n8.exe\",\"12688\",\"Console\",\"1\",\"123,456 K\"\r\n"),
            Some("n8.exe".to_string())
        );
        assert_eq!(parse_tasklist_name("INFO: No tasks are running which match the specified criteria.\r\n"), None);
    }

    #[test]
    fn the_listener_on_the_port_wins_over_the_recorded_daemon() {
        let n8 = |p: u32| Some((p, "n8.exe".to_string()));
        let none: &[u16] = &[];
        // daemon recorded and it is the listener → stop it, clear the pid file
        assert_eq!(plan_stop(Some(7), &[9801, 9803], n8(7), 1), StopPlan::Kill { pid: 7, recorded: true });
        // daemon on another port, a foreground gateway on THIS port → stop only the foreground one
        assert_eq!(plan_stop(Some(7), &[9801], n8(9), 1), StopPlan::Kill { pid: 9, recorded: false });
        // no pid file, foreground gateway → stop it
        assert_eq!(plan_stop(None, none, n8(9), 1), StopPlan::Kill { pid: 9, recorded: false });
        // daemon recorded, nothing on THIS port, daemon serving other ports → hands off
        // (this is the `--stop --port 9811` while the daemon runs on 9801 case)
        assert_eq!(
            plan_stop(Some(7), &[9801, 9803], None, 1),
            StopPlan::Elsewhere { pid: 7, ports: vec![9801, 9803] }
        );
        // daemon recorded, listening nowhere (hung / still starting) → stop it
        assert_eq!(plan_stop(Some(7), none, None, 1), StopPlan::Kill { pid: 7, recorded: true });
        // someone else's port → hands off, even with a daemon recorded elsewhere
        assert_eq!(
            plan_stop(Some(7), &[9801], Some((5, "hyperia-sidecar.exe".to_string())), 1),
            StopPlan::Foreign { pid: 5, name: "hyperia-sidecar.exe".to_string() }
        );
        // never kill ourselves
        assert_eq!(plan_stop(None, none, n8(1), 1), StopPlan::Foreign { pid: 1, name: "n8.exe".to_string() });
        assert_eq!(plan_stop(None, none, None, 1), StopPlan::Nothing);
    }

    #[test]
    fn ports_owned_by_a_pid() {
        let out = "  TCP    0.0.0.0:9801    0.0.0.0:0    LISTENING    30416\n\
  TCP    0.0.0.0:9803    0.0.0.0:0    LISTENING    30416\n\
  TCP    [::]:9801       [::]:0       LISTENING    30416\n\
  TCP    127.0.0.1:9802  0.0.0.0:0    LISTENING    30416\n\
  TCP    0.0.0.0:9800    0.0.0.0:0    LISTENING    46592\n\
  TCP    127.0.0.1:9801  127.0.0.1:5  ESTABLISHED  30416\n";
        assert_eq!(parse_netstat_ports_of(out, 30416), vec![9801, 9802, 9803]);
        assert_eq!(parse_netstat_ports_of(out, 46592), vec![9800]);
        assert!(parse_netstat_ports_of(out, 1).is_empty());
        let ss = "LISTEN 0 1024 0.0.0.0:9801 0.0.0.0:* users:((\"n8\",pid=7,fd=9))\n\
LISTEN 0 1024 0.0.0.0:9803 0.0.0.0:* users:((\"n8\",pid=7,fd=10))\n\
LISTEN 0 128 127.0.0.1:9800 0.0.0.0:* users:((\"hyperia-sidecar\",pid=77,fd=3))\n";
        assert_eq!(parse_ss_ports_of(ss, 7), vec![9801, 9803]);
        assert_eq!(parse_ss_ports_of(ss, 77), vec![9800]);
        // pid=7 must not match pid=77
        assert!(parse_ss_ports_of(ss, 770).is_empty());
    }

    #[test]
    fn only_our_binaries_count_as_the_gateway() {
        assert!(is_n8_process("n8.exe"));
        assert!(is_n8_process("N8.EXE"));
        assert!(is_n8_process("nemesis8"));
        assert!(is_n8_process("C:\\Users\\k\\.local\\bin\\nemesis8.exe"));
        assert!(is_n8_process("/home/k/.local/bin/n8"));
        assert!(!is_n8_process("docker.exe"));
        assert!(!is_n8_process("n8gw"));
        assert!(!is_n8_process(""));
    }
}
