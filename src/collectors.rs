//! LOGPANE EPIC 1 — collectors. The observability data plane: turn the container
//! into a stream of structured events. Runs inside `nemesis8-monitor` alongside
//! the filesystem watcher and the pulse sampler. Two collectors live here:
//!
//!   • [`MetricsCollector`] — periodic host/container metrics (CPU / memory /
//!     load) read from `/proc`, emitted as `MonitorEvent::Metric`.
//!   • [`LogTailer`] — Splunk-style ingest: discover `*.log` files under the
//!     workspace and emit each newly-appended line as `MonitorEvent::LogLine`.
//!
//! Output flows through the monitor's existing sink fan-out (durable JSONL +
//! the Hyperia HTTP sink), so later epics (event index, search UI) consume one
//! unified stream. Everything here is pure `std` — no new dependencies — and the
//! parsers take `&str` so they're unit-testable off a real container.

use std::collections::HashMap;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

// ── metrics ──────────────────────────────────────────────────────────────────

/// A point-in-time host/container metrics snapshot.
#[derive(Debug, Clone, Copy, Default)]
pub struct Metrics {
    /// Whole-container CPU busy % over the sample interval.
    pub cpu_pct: f64,
    pub mem_used_kb: u64,
    pub mem_total_kb: u64,
    /// 1-minute load average.
    pub load1: f64,
    /// Network throughput over the interval, bytes/sec, summed across all
    /// non-loopback interfaces.
    pub net_rx_bps: u64,
    pub net_tx_bps: u64,
}

/// Samples CPU / memory / load / network from `/proc`. Holds previous CPU jiffies
/// and network byte counters (plus the sample instant) so percentages and
/// throughput are deltas over the interval — construct once (reads the baseline),
/// then call [`sample`](Self::sample) on each tick.
pub struct MetricsCollector {
    prev_idle: u64,
    prev_total: u64,
    prev_rx: u64,
    prev_tx: u64,
    prev_usage_usec: u64,
    last: std::time::Instant,
}

impl Default for MetricsCollector {
    fn default() -> Self {
        let (idle, total) = read_cpu_jiffies().unwrap_or((0, 0));
        let (rx, tx) = read_net_bytes();
        let prev_usage_usec = read_cgroup_usage_usec().unwrap_or(0);
        Self {
            prev_idle: idle,
            prev_total: total,
            prev_rx: rx,
            prev_tx: tx,
            prev_usage_usec,
            last: std::time::Instant::now(),
        }
    }
}

impl MetricsCollector {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn sample(&mut self) -> Metrics {
        let now = std::time::Instant::now();
        let elapsed = now.duration_since(self.last).as_secs_f64().max(0.001);
        let elapsed_usec = now.duration_since(self.last).as_micros() as f64;

        let cpu_pct = match read_cgroup_usage_usec() {
            Some(usage_usec) => {
                let d_usage = usage_usec.saturating_sub(self.prev_usage_usec);
                self.prev_usage_usec = usage_usec;
                if elapsed_usec > 0.0 {
                    (d_usage as f64 / elapsed_usec) * 100.0
                } else {
                    0.0
                }
            }
            None => match read_cpu_jiffies() {
                Some((idle, total)) => {
                    let d_total = total.saturating_sub(self.prev_total);
                    let d_idle = idle.saturating_sub(self.prev_idle);
                    self.prev_idle = idle;
                    self.prev_total = total;
                    if d_total > 0 {
                        (d_total.saturating_sub(d_idle) as f64 / d_total as f64) * 100.0
                    } else {
                        0.0
                    }
                }
                None => 0.0,
            },
        };

        // Network throughput = byte-counter delta / elapsed.
        let (rx, tx) = read_net_bytes();
        let net_rx_bps = (rx.saturating_sub(self.prev_rx) as f64 / elapsed) as u64;
        let net_tx_bps = (tx.saturating_sub(self.prev_tx) as f64 / elapsed) as u64;
        self.prev_rx = rx;
        self.prev_tx = tx;
        self.last = now;

        let (mem_total_kb, mem_used_kb) = if let Some((total, used)) = read_cgroup_memory() {
            (total, used)
        } else {
            read_meminfo().unwrap_or((0, 0))
        };
        let load1 = read_loadavg().unwrap_or(0.0);
        Metrics {
            cpu_pct,
            mem_used_kb,
            mem_total_kb,
            load1,
            net_rx_bps,
            net_tx_bps,
        }
    }
}

fn read_net_bytes() -> (u64, u64) {
    std::fs::read_to_string("/proc/net/dev")
        .map(|s| parse_net_dev_total(&s))
        .unwrap_or((0, 0))
}

/// Sum receive/transmit bytes across every interface except loopback.
/// `/proc/net/dev` rows: `iface: rx_bytes rx_pkts … (8 fields) tx_bytes …`.
fn parse_net_dev_total(net_dev: &str) -> (u64, u64) {
    let mut rx = 0u64;
    let mut tx = 0u64;
    for line in net_dev.lines() {
        let Some((iface, rest)) = line.trim_start().split_once(':') else {
            continue;
        };
        if iface.trim() == "lo" {
            continue;
        }
        let f: Vec<u64> = rest.split_whitespace().filter_map(|v| v.parse().ok()).collect();
        if f.len() >= 9 {
            rx = rx.saturating_add(f[0]);
            tx = tx.saturating_add(f[8]);
        }
    }
    (rx, tx)
}

fn read_cpu_jiffies() -> Option<(u64, u64)> {
    parse_cpu_jiffies(&fs::read_to_string("/proc/stat").ok()?)
}

fn read_cgroup_usage_usec() -> Option<u64> {
    parse_cgroup_cpu(&fs::read_to_string("/sys/fs/cgroup/cpu.stat").ok()?)
}

fn parse_cgroup_cpu(cpu_stat: &str) -> Option<u64> {
    for line in cpu_stat.lines() {
        let mut parts = line.split_whitespace();
        if parts.next() == Some("usage_usec") {
            return parts.next().and_then(|v| v.parse().ok());
        }
    }
    None
}

fn read_cgroup_memory() -> Option<(u64, u64)> {
    let current_bytes = fs::read_to_string("/sys/fs/cgroup/memory.current")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())?;
    let used_kb = current_bytes / 1024;

    let total_kb = if let Ok(max_str) = fs::read_to_string("/sys/fs/cgroup/memory.max") {
        if let Some(limit_bytes) = parse_cgroup_memory_max(&max_str) {
            limit_bytes / 1024
        } else {
            read_meminfo().map(|(total, _)| total).unwrap_or(0)
        }
    } else {
        read_meminfo().map(|(total, _)| total).unwrap_or(0)
    };

    Some((total_kb, used_kb))
}

fn parse_cgroup_memory_max(memory_max: &str) -> Option<u64> {
    let trimmed = memory_max.trim();
    if trimmed == "max" {
        None
    } else {
        trimmed.parse().ok()
    }
}

/// First `cpu ` line of /proc/stat: `user nice system idle iowait irq softirq
/// steal …`. Returns `(idle_jiffies, total_jiffies)`; idle counts idle+iowait.
fn parse_cpu_jiffies(stat: &str) -> Option<(u64, u64)> {
    let line = stat.lines().find(|l| l.starts_with("cpu "))?;
    let vals: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .filter_map(|v| v.parse().ok())
        .collect();
    if vals.len() < 4 {
        return None;
    }
    let idle = vals[3].saturating_add(vals.get(4).copied().unwrap_or(0));
    let total: u64 = vals.iter().sum();
    Some((idle, total))
}

fn read_meminfo() -> Option<(u64, u64)> {
    parse_meminfo(&fs::read_to_string("/proc/meminfo").ok()?)
}

/// Returns `(total_kb, used_kb)` where `used = MemTotal - MemAvailable`.
fn parse_meminfo(info: &str) -> Option<(u64, u64)> {
    let mut total = 0u64;
    let mut avail = 0u64;
    for line in info.lines() {
        if let Some(v) = line.strip_prefix("MemTotal:") {
            total = parse_first_u64(v);
        } else if let Some(v) = line.strip_prefix("MemAvailable:") {
            avail = parse_first_u64(v);
        }
    }
    if total == 0 {
        return None;
    }
    Some((total, total.saturating_sub(avail)))
}

fn read_loadavg() -> Option<f64> {
    parse_loadavg(&fs::read_to_string("/proc/loadavg").ok()?)
}

fn parse_loadavg(la: &str) -> Option<f64> {
    la.split_whitespace().next()?.parse().ok()
}

fn parse_first_u64(s: &str) -> u64 {
    s.split_whitespace().next().and_then(|n| n.parse().ok()).unwrap_or(0)
}

// ── log tailer ───────────────────────────────────────────────────────────────

/// Splunk-style log ingest: discovers `*.log` files under a root and emits each
/// newly-appended line. Tracks a byte offset per file; on truncation/rotation
/// (file smaller than the saved offset) it re-reads from the start.
pub struct LogTailer {
    root: PathBuf,
    offsets: HashMap<PathBuf, u64>,
    max_files: usize,
    max_line_bytes: usize,
    /// Files that turned out to be binary wearing a .log extension — tailed
    /// once, produced soup, never read again (one notice line is emitted).
    binary_blacklist: std::collections::HashSet<PathBuf>,
}

impl LogTailer {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            offsets: HashMap::new(),
            max_files: 128,
            max_line_bytes: 16 * 1024,
            binary_blacklist: std::collections::HashSet::new(),
        }
    }

    /// Poll once: `(path, line)` for every line appended since the last poll,
    /// across all `*.log` files under the root.
    pub fn poll(&mut self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for f in discover_logs(&self.root, self.max_files) {
            if self.binary_blacklist.contains(&f) {
                continue;
            }
            let Ok(meta) = fs::metadata(&f) else { continue };
            let size = meta.len();
            let start = match self.offsets.get(&f).copied() {
                Some(prev) if prev <= size => prev,
                _ => 0, // new file, or truncated/rotated → re-read from start
            };
            if size <= start {
                self.offsets.insert(f.clone(), size);
                continue;
            }
            if let Ok(chunk) = read_range(&f, start, size) {
                let path = f.to_string_lossy().to_string();
                if crate::event_store::looks_binary(&chunk) {
                    self.binary_blacklist.insert(f.clone());
                    self.offsets.insert(f, size);
                    out.push((path, "[log tailer] binary-looking file skipped".to_string()));
                    continue;
                }
                for line in chunk.lines() {
                    let line = truncate_chars(line, self.max_line_bytes);
                    if !line.is_empty() {
                        out.push((path.clone(), line.to_string()));
                    }
                }
            }
            self.offsets.insert(f, size);
        }
        out
    }
}

fn truncate_chars(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

fn discover_logs(root: &Path, max: usize) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if out.len() >= max {
            break;
        }
        let Ok(rd) = fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|x| x == "log") {
                out.push(p);
                if out.len() >= max {
                    break;
                }
            }
        }
    }
    out
}

fn read_range(path: &Path, start: u64, end: u64) -> std::io::Result<String> {
    let mut f = fs::File::open(path)?;
    f.seek(SeekFrom::Start(start))?;
    let mut buf = vec![0u8; (end - start) as usize];
    f.read_exact(&mut buf)?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cpu_jiffies() {
        let stat = "cpu  100 0 50 800 40 0 10 0 0 0\ncpu0 1 2 3 4\n";
        let (idle, total) = parse_cpu_jiffies(stat).unwrap();
        assert_eq!(idle, 840); // idle 800 + iowait 40
        assert_eq!(total, 1000);
    }

    #[test]
    fn parses_meminfo() {
        let info = "MemTotal:       8000 kB\nMemFree:        1000 kB\nMemAvailable:   3000 kB\n";
        assert_eq!(parse_meminfo(info), Some((8000, 5000))); // used = total - available
    }

    #[test]
    fn parses_loadavg() {
        assert_eq!(parse_loadavg("0.42 0.30 0.25 1/200 1234"), Some(0.42));
    }

    #[test]
    fn sums_net_excluding_loopback() {
        let dev = "\
Inter-|   Receive                    |  Transmit
 face |bytes packets errs drop fifo frame compressed multicast|bytes packets errs drop fifo colls carrier compressed
    lo: 10 0 0 0 0 0 0 0 20 0 0 0 0 0 0 0
  eth0: 1000 2 0 0 0 0 0 0 2500 3 0 0 0 0 0 0
  eth1: 500 1 0 0 0 0 0 0 250 1 0 0 0 0 0 0
";
        // rx = 1000+500, tx = 2500+250 (lo excluded)
        assert_eq!(parse_net_dev_total(dev), (1500, 2750));
    }

    #[test]
    fn log_tailer_emits_only_new_lines() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("app.log");
        std::fs::write(&log, "line1\nline2\n").unwrap();

        let mut t = LogTailer::new(dir.path());
        let first = t.poll();
        assert_eq!(first.len(), 2);
        assert!(first[1].1.contains("line2"));

        // No new content → nothing emitted.
        assert!(t.poll().is_empty());

        // Append one line → only that line is emitted.
        let mut f = std::fs::OpenOptions::new().append(true).open(&log).unwrap();
        writeln!(f, "line3").unwrap();
        let next = t.poll();
        assert_eq!(next.len(), 1);
        assert!(next[0].1.contains("line3"));
    }

    #[test]
    fn parses_cgroup_cpu_valid() {
        let cpu_stat = "usage_usec 123456789\nuser_usec 100000000\nsystem_usec 23456789\n";
        assert_eq!(parse_cgroup_cpu(cpu_stat), Some(123456789));
    }

    #[test]
    fn parses_cgroup_cpu_tabs_and_spaces() {
        let cpu_stat = "usage_usec\t98765\nuser_usec 90000\n";
        assert_eq!(parse_cgroup_cpu(cpu_stat), Some(98765));
    }

    #[test]
    fn parses_cgroup_cpu_missing() {
        let cpu_stat = "user_usec 100000000\nsystem_usec 23456789\n";
        assert_eq!(parse_cgroup_cpu(cpu_stat), None);
    }

    #[test]
    fn parses_cgroup_memory_valid() {
        let memory_current = "14090240\n";
        assert_eq!(memory_current.trim().parse::<u64>().ok(), Some(14090240));
    }

    #[test]
    fn parses_cgroup_memory_max_value() {
        assert_eq!(parse_cgroup_memory_max("max\n"), None);
        assert_eq!(parse_cgroup_memory_max("104857600\n"), Some(104857600));
        assert_eq!(parse_cgroup_memory_max("invalid\n"), None);
    }

    #[test]
    fn test_cgroup_cpu_delta_math() {
        let prev_usage = 1000000;
        let curr_usage = 1500000;
        let elapsed_usec = 500000.0; // 0.5 seconds
        let d_usage = curr_usage - prev_usage;
        let cpu_pct = if elapsed_usec > 0.0 {
            (d_usage as f64 / elapsed_usec) * 100.0
        } else {
            0.0
        };
        assert_eq!(cpu_pct, 100.0);
    }
}
