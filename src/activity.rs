use std::collections::{HashMap, HashSet};
use std::fs;
use std::time::Instant;

const PROC_DIR: &str = "/proc";
const NET_DEV: &str = "/proc/net/dev";
const CGROUP_PROCS: &str = "/sys/fs/cgroup/cgroup.procs";
const DEFAULT_CLK_TCK: f64 = 100.0;

pub struct ActivitySample {
    pub subprocs: u32,
    pub cpu_pct: f64,
    pub net_bps: u64,
    pub io_bps: u64,
}

pub struct ActivityState {
    pub prev_cpu_ticks: u64,
    pub prev_net_bytes: u64,
    pub prev_io_bytes: u64,
    /// Pids that looked like agent-spawned work last sample. `subprocs` counts
    /// pids that are NEW vs this set — so the persistent agent CLI/entry/monitor
    /// don't read as busy; only freshly-exec'd tool runs do.
    pub prev_pids: HashSet<u32>,
    pub last: Instant,
}

impl Default for ActivityState {
    fn default() -> Self {
        let counters = read_counters();
        Self {
            prev_cpu_ticks: counters.cpu_ticks,
            prev_net_bytes: counters.net_bytes,
            prev_io_bytes: counters.io_bytes,
            prev_pids: counters.subproc_pids,
            last: Instant::now(),
        }
    }
}

impl ActivitySample {
    pub fn busy(&self, cpu_min: f64, net_min: u64, io_min: u64) -> bool {
        self.subprocs > 0
            || self.cpu_pct > cpu_min
            || self.net_bps > net_min
            || self.io_bps > io_min
    }
}

pub fn sample(prev: &mut ActivityState) -> ActivitySample {
    let now = Instant::now();
    let elapsed = now.duration_since(prev.last).as_secs_f64().max(0.001);
    let counters = read_counters();

    let cpu_delta = counters.cpu_ticks.saturating_sub(prev.prev_cpu_ticks);
    let net_delta = counters.net_bytes.saturating_sub(prev.prev_net_bytes);
    let io_delta = counters.io_bytes.saturating_sub(prev.prev_io_bytes);
    // Count pids that are NEW since last sample (freshly-spawned tool runs).
    // A long-running tool stops being "new" after its first sample — but its CPU/
    // net/IO keep it busy via the other signals; only the idle CLI (persistent,
    // quiet) reads as idle.
    let new_subprocs = counters.subproc_pids.difference(&prev.prev_pids).count() as u32;

    prev.prev_cpu_ticks = counters.cpu_ticks;
    prev.prev_net_bytes = counters.net_bytes;
    prev.prev_io_bytes = counters.io_bytes;
    prev.prev_pids = counters.subproc_pids;
    prev.last = now;

    ActivitySample {
        subprocs: new_subprocs,
        cpu_pct: (cpu_delta as f64 / DEFAULT_CLK_TCK / elapsed) * 100.0,
        net_bps: bytes_per_sec(net_delta, elapsed),
        io_bps: bytes_per_sec(io_delta, elapsed),
    }
}

struct Counters {
    /// Pids that look like agent-spawned work (filtered process set), used for
    /// the new-pid diff in `sample`.
    subproc_pids: HashSet<u32>,
    cpu_ticks: u64,
    net_bytes: u64,
    io_bytes: u64,
}

fn read_counters() -> Counters {
    let procs = read_processes();
    Counters {
        subproc_pids: real_subproc_pids(&procs),
        cpu_ticks: procs.iter().map(|p| p.utime + p.stime).sum(),
        net_bytes: read_eth0_bytes(),
        io_bytes: procs.iter().map(|p| p.io_bytes).sum(),
    }
}

fn bytes_per_sec(delta: u64, elapsed: f64) -> u64 {
    (delta as f64 / elapsed).max(0.0).round() as u64
}

#[derive(Debug)]
struct ProcInfo {
    pid: u32,
    ppid: u32,
    utime: u64,
    stime: u64,
    io_bytes: u64,
}

fn read_processes() -> Vec<ProcInfo> {
    let mut out = Vec::new();
    let cgroup_pids = read_cgroup_pids();
    let entries = match fs::read_dir(PROC_DIR) {
        Ok(entries) => entries,
        Err(_) => return out,
    };

    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Ok(pid) = name.parse::<u32>() else {
            continue;
        };
        if let Some(pids) = &cgroup_pids {
            if !pids.contains(&pid) {
                continue;
            }
        }
        let stat_path = entry.path().join("stat");
        let io_path = entry.path().join("io");
        let Ok(stat) = fs::read_to_string(stat_path) else {
            continue;
        };
        let Some((ppid, utime, stime)) = parse_proc_stat(&stat) else {
            continue;
        };
        out.push(ProcInfo {
            pid,
            ppid,
            utime,
            stime,
            io_bytes: fs::read_to_string(io_path)
                .ok()
                .map(|s| parse_proc_io_bytes(&s))
                .unwrap_or(0),
        });
    }

    out
}

/// Pids that look like agent-spawned work: descendants of pid1 below the top
/// level (the agent CLI is a top-level child of tini via entry, so it and its
/// long-lived peers are included here but filtered out by the new-pid diff in
/// `sample`), minus pid1 and the monitor's own process tree.
fn real_subproc_pids(procs: &[ProcInfo]) -> HashSet<u32> {
    let self_pid = std::process::id();
    let mut parent_by_pid = HashMap::new();
    for proc in procs {
        parent_by_pid.insert(proc.pid, proc.ppid);
    }

    let monitor_tree = descendants_of(self_pid, procs);
    procs
        .iter()
        .filter(|proc| proc.pid != 1)
        .filter(|proc| proc.pid != self_pid)
        .filter(|proc| !monitor_tree.contains(&proc.pid))
        .filter(|proc| is_descendant_of_pid1_below_top_level(proc.pid, &parent_by_pid))
        .map(|proc| proc.pid)
        .collect()
}

fn descendants_of(root: u32, procs: &[ProcInfo]) -> HashSet<u32> {
    let mut children_by_parent: HashMap<u32, Vec<u32>> = HashMap::new();
    for proc in procs {
        children_by_parent
            .entry(proc.ppid)
            .or_default()
            .push(proc.pid);
    }

    let mut seen = HashSet::new();
    let mut stack = children_by_parent.remove(&root).unwrap_or_default();
    while let Some(pid) = stack.pop() {
        if !seen.insert(pid) {
            continue;
        }
        if let Some(children) = children_by_parent.get(&pid) {
            stack.extend(children.iter().copied());
        }
    }
    seen
}

fn read_cgroup_pids() -> Option<HashSet<u32>> {
    let text = fs::read_to_string(CGROUP_PROCS).ok()?;
    Some(
        text.lines()
            .filter_map(|line| line.trim().parse::<u32>().ok())
            .collect(),
    )
}

fn is_descendant_of_pid1_below_top_level(pid: u32, parent_by_pid: &HashMap<u32, u32>) -> bool {
    let mut current = pid;
    let mut depth = 0_u32;

    while let Some(&parent) = parent_by_pid.get(&current) {
        if parent == 1 {
            return depth >= 1;
        }
        if parent == 0 || parent == current {
            return false;
        }
        current = parent;
        depth += 1;
        if depth > 256 {
            return false;
        }
    }

    false
}

fn parse_proc_stat(stat: &str) -> Option<(u32, u64, u64)> {
    let close = stat.rfind(')')?;
    let rest = stat.get(close + 2..)?;
    let fields: Vec<&str> = rest.split_whitespace().collect();
    let ppid = fields.get(1)?.parse().ok()?;
    let utime = fields.get(11)?.parse().ok()?;
    let stime = fields.get(12)?.parse().ok()?;
    Some((ppid, utime, stime))
}

fn read_eth0_bytes() -> u64 {
    fs::read_to_string(NET_DEV)
        .ok()
        .and_then(|s| parse_net_dev_eth0_bytes(&s))
        .unwrap_or(0)
}

fn parse_net_dev_eth0_bytes(net_dev: &str) -> Option<u64> {
    for line in net_dev.lines() {
        let trimmed = line.trim_start();
        let Some(rest) = trimmed.strip_prefix("eth0:") else {
            continue;
        };
        let fields: Vec<&str> = rest.split_whitespace().collect();
        let rx_bytes: u64 = fields.first()?.parse().ok()?;
        let tx_bytes: u64 = fields.get(8)?.parse().ok()?;
        return Some(rx_bytes.saturating_add(tx_bytes));
    }
    None
}

fn parse_proc_io_bytes(io: &str) -> u64 {
    let mut read_bytes = 0_u64;
    let mut write_bytes = 0_u64;

    for line in io.lines() {
        if let Some(value) = line.strip_prefix("read_bytes:") {
            read_bytes = value.trim().parse().unwrap_or(0);
        } else if let Some(value) = line.strip_prefix("write_bytes:") {
            write_bytes = value.trim().parse().unwrap_or(0);
        }
    }

    read_bytes.saturating_add(write_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_proc_and_net_counters() {
        let stat = "42 (agent worker) S 7 1 1 0 -1 4194560 12 0 0 0 123 45 0 0 20 0 1 0 100";
        assert_eq!(parse_proc_stat(stat), Some((7, 123, 45)));

        let net = "\
Inter-|   Receive                                                |  Transmit
 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed
  lo: 10 0 0 0 0 0 0 0 20 0 0 0 0 0 0 0
eth0: 1000 2 0 0 0 0 0 0 2500 3 0 0 0 0 0 0
";
        assert_eq!(parse_net_dev_eth0_bytes(net), Some(3500));

        let io = "\
rchar: 1
wchar: 2
read_bytes: 4096
write_bytes: 8192
cancelled_write_bytes: 1024
";
        assert_eq!(parse_proc_io_bytes(io), 12_288);
    }
}
