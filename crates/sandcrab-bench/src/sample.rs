//! Host-side resource readers (bench-spec §2): cgroup v2 files, `/proc/<pid>/smaps_rollup`
//! PSS, `/proc/<pid>/stat` CPU ticks. Pure `/proc` + `/sys` reads, no root assumed: an
//! unreadable value is an `Err(reason)` the caller records as not measured.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use crate::host::{cmd, read_trim};

/// A cgroup v2 snapshot.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CgroupSample {
    pub memory_current: u64,
    pub memory_peak: Option<u64>,
    pub anon: u64,
    pub file: u64,
    pub kernel: u64,
    pub cpu_usage_usec: u64,
}

fn kv(text: &str, key: &str) -> Option<u64> {
    text.lines().find_map(|l| {
        let (k, v) = l.split_once(' ')?;
        (k == key).then(|| v.trim().parse().ok())?
    })
}

pub fn read_cgroup(dir: &Path) -> Result<CgroupSample, String> {
    let current = read_trim(dir.join("memory.current"))
        .ok_or_else(|| format!("cannot read {}/memory.current", dir.display()))?
        .parse()
        .map_err(|e| format!("memory.current: {e}"))?;
    let stat = fs::read_to_string(dir.join("memory.stat")).unwrap_or_default();
    let cpu = fs::read_to_string(dir.join("cpu.stat")).unwrap_or_default();
    Ok(CgroupSample {
        memory_current: current,
        memory_peak: read_trim(dir.join("memory.peak")).and_then(|v| v.parse().ok()),
        anon: kv(&stat, "anon").unwrap_or(0),
        file: kv(&stat, "file").unwrap_or(0),
        kernel: kv(&stat, "kernel").unwrap_or(0),
        cpu_usage_usec: kv(&cpu, "usage_usec").unwrap_or(0),
    })
}

/// Container cgroup dir from `/proc/<pid>/cgroup` (`0::/system.slice/docker-<id>.scope`),
/// falling back to the systemd / cgroupfs driver layouts.
#[must_use]
pub fn cgroup_dir(pid: Option<u32>, container_id: &str) -> Option<PathBuf> {
    let root = Path::new("/sys/fs/cgroup");
    if let Some(pid) = pid
        && let Ok(text) = fs::read_to_string(format!("/proc/{pid}/cgroup"))
        && let Some(rel) = text.lines().find_map(|l| l.strip_prefix("0::"))
    {
        let p = root.join(rel.trim_start_matches('/'));
        if p.join("memory.current").exists() {
            return Some(p);
        }
    }
    [
        root.join(format!("system.slice/docker-{container_id}.scope")),
        root.join(format!("docker/{container_id}")),
    ]
    .into_iter()
    .find(|p| p.exists())
}

/// PSS (bytes) of one process from `smaps_rollup`.
pub fn pss(pid: u32) -> Result<u64, String> {
    let path = format!("/proc/{pid}/smaps_rollup");
    let text = fs::read_to_string(&path).map_err(|e| format!("{path}: {e}"))?;
    text.lines()
        .find_map(|l| {
            let rest = l.strip_prefix("Pss:")?;
            rest.split_whitespace().next()?.parse::<u64>().ok()
        })
        .map(|kb| kb * 1024)
        .ok_or_else(|| format!("{path}: no Pss line"))
}

/// Summed PSS of every process in a cgroup.
pub fn cgroup_pss(dir: &Path) -> Result<u64, String> {
    let procs = fs::read_to_string(dir.join("cgroup.procs"))
        .map_err(|e| format!("{}/cgroup.procs: {e}", dir.display()))?;
    let mut total = 0;
    for pid in procs.lines().filter_map(|l| l.trim().parse::<u32>().ok()) {
        match pss(pid) {
            Ok(v) => total += v,
            // exited between listing and reading
            Err(_) if !Path::new(&format!("/proc/{pid}")).exists() => {}
            Err(e) => return Err(e),
        }
    }
    Ok(total)
}

/// `utime + stime` ticks of a process.
#[must_use]
pub fn proc_cpu_ticks(pid: u32) -> Option<u64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    parse_stat_ticks(&stat)
}

fn parse_stat_ticks(stat: &str) -> Option<u64> {
    // fields after the `(comm)`: state(3) ppid(4) ... utime(14) stime(15)
    let after = stat.rsplit_once(')')?.1;
    let f: Vec<&str> = after.split_whitespace().collect();
    let utime: u64 = f.get(11)?.parse().ok()?;
    let stime: u64 = f.get(12)?.parse().ok()?;
    Some(utime + stime)
}

#[must_use]
pub fn proc_ppid(pid: u32) -> Option<u32> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat.rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

/// Resident set size (bytes) of one process, on any host: `VmRSS` on Linux, `ps -o rss=` on
/// macOS (no cgroups, no PSS there). **RSS, not PSS**: shared pages (the libkrun/libkrunfw
/// dylibs, the guest RAM's resident part) count fully, so it is reported under its own
/// `*_rss*` metric ids and never mixed with the Linux PSS columns (design §13).
pub fn rss(pid: u32) -> Result<u64, String> {
    #[cfg(target_os = "linux")]
    {
        proc_rss(pid).ok_or_else(|| format!("/proc/{pid}/status: no VmRSS"))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let out = cmd("ps", &["-o", "rss=", "-p", &pid.to_string()])
            .ok_or_else(|| format!("ps -o rss= -p {pid} failed (process gone?)"))?;
        parse_ps_rss_kib(&out).ok_or_else(|| format!("ps -o rss=: unexpected output {out:?}"))
    }
}

/// Total CPU time (user + system, all threads; seconds) of one process: `/proc/<pid>/stat`
/// ticks on Linux, `ps -o time=` on macOS (10 ms resolution). For a libkrun helper this
/// includes the vCPU threads, i.e. the guest's CPU as seen by the host.
pub fn process_cpu_s(pid: u32) -> Result<f64, String> {
    #[cfg(target_os = "linux")]
    {
        proc_cpu_ticks(pid)
            .map(ticks_to_s)
            .ok_or_else(|| format!("/proc/{pid}/stat unreadable"))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let out = cmd("ps", &["-o", "time=", "-p", &pid.to_string()])
            .ok_or_else(|| format!("ps -o time= -p {pid} failed (process gone?)"))?;
        parse_ps_time(&out).ok_or_else(|| format!("ps -o time=: unexpected output {out:?}"))
    }
}

/// `ps -o time=`: `[[dd-]hh:]mm:ss.cc` → seconds (either decimal separator).
#[cfg_attr(target_os = "linux", allow(dead_code))]
fn parse_ps_time(out: &str) -> Option<f64> {
    let t = out.trim().replace(',', ".");
    let (days, rest) = match t.split_once('-') {
        Some((d, r)) => (d.parse::<f64>().ok()?, r.to_owned()),
        None => (0.0, t),
    };
    let mut secs = 0.0;
    for part in rest.split(':') {
        secs = secs * 60.0 + part.parse::<f64>().ok()?;
    }
    Some(days * 86_400.0 + secs)
}

/// `ps -o rss=` prints KiB.
#[cfg_attr(target_os = "linux", allow(dead_code))]
fn parse_ps_rss_kib(out: &str) -> Option<u64> {
    out.trim().parse::<u64>().ok().map(|kib| kib * 1024)
}

/// Is `pid` still running (and not a zombie)?
#[must_use]
pub fn process_alive(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|s| {
                s.rsplit_once(')')
                    .map(|(_, rest)| rest.trim_start().to_owned())
            })
            .is_some_and(|rest| !rest.starts_with('Z'))
    }
    #[cfg(not(target_os = "linux"))]
    {
        cmd("ps", &["-o", "stat=", "-p", &pid.to_string()])
            .is_some_and(|s| !s.trim().is_empty() && !s.trim_start().starts_with('Z'))
    }
}

/// `VmRSS` (bytes) from `/proc/<pid>/status` (world-readable, unlike smaps).
#[must_use]
pub fn proc_rss(pid: u32) -> Option<u64> {
    let s = fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    s.lines()
        .find_map(|l| l.strip_prefix("VmRSS:"))
        .and_then(|v| v.split_whitespace().next()?.parse::<u64>().ok())
        .map(|kb| kb * 1024)
}

#[must_use]
pub fn pidof(name: &str) -> Option<u32> {
    cmd("pidof", &["-s", name]).and_then(|s| s.trim().parse().ok())
}

#[must_use]
pub fn clk_tck() -> u64 {
    static TCK: OnceLock<u64> = OnceLock::new();
    *TCK.get_or_init(|| {
        cmd("getconf", &["CLK_TCK"])
            .and_then(|s| s.parse().ok())
            .unwrap_or(100)
    })
}

#[must_use]
pub fn ticks_to_s(ticks: u64) -> f64 {
    ticks as f64 / clk_tck() as f64
}

/// On-CPU ns of the calling thread (`/proc/thread-self/schedstat`, field 1).
#[must_use]
pub fn thread_cpu_ns() -> Option<u64> {
    read_trim("/proc/thread-self/schedstat")?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

/// Sample `memory.current` at 10 Hz for `window`; returns the samples.
pub async fn sample_memory(dir: &Path, window: Duration) -> Vec<u64> {
    let mut out = Vec::new();
    let end = Instant::now() + window;
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    while Instant::now() < end {
        tick.tick().await;
        if let Ok(s) = read_cgroup(dir) {
            out.push(s.memory_current);
        }
    }
    out
}

/// Sampler self-overhead: run the 10 Hz cgroup read loop on a dedicated thread for `window`
/// and return its CPU use as % of one core.
#[must_use]
pub fn sampler_cpu_pct(dir: &Path, window: Duration) -> f64 {
    let dir = dir.to_path_buf();
    std::thread::spawn(move || {
        let (Some(c0), t0) = (thread_cpu_ns(), Instant::now()) else {
            return f64::NAN;
        };
        while t0.elapsed() < window {
            let _ = read_cgroup(&dir);
            std::thread::sleep(Duration::from_millis(100));
        }
        let c1 = thread_cpu_ns().unwrap_or(c0);
        (c1 - c0) as f64 / t0.elapsed().as_nanos() as f64 * 100.0
    })
    .join()
    .unwrap_or(f64::NAN)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ps_time_parses_all_shapes() {
        assert_eq!(parse_ps_time("0:00.12"), Some(0.12));
        assert_eq!(parse_ps_time(" 1:02.50\n"), Some(62.5));
        assert_eq!(parse_ps_time("0:01,25"), Some(1.25));
        assert_eq!(parse_ps_time("1:00:00.00"), Some(3600.0));
        assert_eq!(parse_ps_time("2-00:00:01"), Some(172_801.0));
        assert_eq!(parse_ps_time("garbage"), None);
    }

    #[test]
    fn stat_ticks_with_spaces_in_comm() {
        let stat = "1234 (my (odd) proc) S 1 1234 1234 0 -1 4194560 100 0 0 0 7 5 0 0 20 0 1 0";
        assert_eq!(parse_stat_ticks(stat), Some(12));
    }

    #[test]
    fn ps_rss_is_kib() {
        assert_eq!(parse_ps_rss_kib("  2048\n"), Some(2 * 1024 * 1024));
        assert_eq!(parse_ps_rss_kib(""), None);
    }

    #[test]
    fn rss_of_self() {
        let me = std::process::id();
        assert!(rss(me).unwrap() > 0);
        assert!(process_alive(me));
    }

    #[test]
    fn kv_parses_stat_files() {
        let t = "anon 4096\nfile 8192\nkernel 12\n";
        assert_eq!(kv(t, "file"), Some(8192));
        assert_eq!(kv(t, "nope"), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn self_readers_work() {
        let me = std::process::id();
        assert!(proc_cpu_ticks(me).is_some());
        assert!(proc_rss(me).unwrap() > 0);
        assert!(pss(me).unwrap() > 0);
        assert!(thread_cpu_ns().is_some());
    }
}
