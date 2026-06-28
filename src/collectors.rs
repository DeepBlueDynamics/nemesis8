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
}

/// Samples CPU / memory / load from `/proc`. Holds the previous CPU jiffies so
/// the busy percentage is a delta over the interval — construct once (reads the
/// baseline), then call [`sample`](Self::sample) on each tick.
pub struct MetricsCollector {
    prev_idle: u64,
    prev_total: u64,
}

impl Default for MetricsCollector {
    fn default() -> Self {
        let (idle, total) = read_cpu_jiffies().unwrap_or((0, 0));
        Self { prev_idle: idle, prev_total: total }
    }
}

impl MetricsCollector {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn sample(&mut self) -> Metrics {
        let cpu_pct = match read_cpu_jiffies() {
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
        };
        let (mem_total_kb, mem_used_kb) = read_meminfo().unwrap_or((0, 0));
        let load1 = read_loadavg().unwrap_or(0.0);
        Metrics { cpu_pct, mem_used_kb, mem_total_kb, load1 }
    }
}

fn read_cpu_jiffies() -> Option<(u64, u64)> {
    parse_cpu_jiffies(&fs::read_to_string("/proc/stat").ok()?)
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
}

impl LogTailer {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            offsets: HashMap::new(),
            max_files: 128,
            max_line_bytes: 16 * 1024,
        }
    }

    /// Poll once: `(path, line)` for every line appended since the last poll,
    /// across all `*.log` files under the root.
    pub fn poll(&mut self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for f in discover_logs(&self.root, self.max_files) {
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
}
