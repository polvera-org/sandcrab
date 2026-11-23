//! Host declaration and noise capture (bench-spec §4, §5, §6). Everything here is
//! best-effort and read-only: a value that cannot be read is recorded as empty/`unknown`,
//! never guessed.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::process::Command;

use sha2::{Digest, Sha256};

use crate::results::{
    CrabdInfo, DockerInfo, FirecrackerInfo, GitInfo, HostInfo, Ksm, Noise, Storage,
};

/// stdout of a command, trimmed; `None` if it cannot run or fails.
pub fn cmd(program: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(program).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

pub fn read_trim(path: impl AsRef<Path>) -> Option<String> {
    fs::read_to_string(path).ok().map(|s| s.trim().to_owned())
}

pub fn sha256_file(path: &Path) -> Option<String> {
    let bytes = fs::read(path).ok()?;
    Some(format!("sha256:{:x}", Sha256::digest(&bytes)))
}

/// `/proc/meminfo` in bytes.
#[must_use]
pub fn meminfo() -> BTreeMap<String, u64> {
    parse_meminfo(&fs::read_to_string("/proc/meminfo").unwrap_or_default())
}

#[must_use]
pub fn parse_meminfo(text: &str) -> BTreeMap<String, u64> {
    text.lines()
        .filter_map(|l| {
            let (k, rest) = l.split_once(':')?;
            let mut it = rest.split_whitespace();
            let v: u64 = it.next()?.parse().ok()?;
            let mult = if it.next() == Some("kB") { 1024 } else { 1 };
            Some((k.to_owned(), v * mult))
        })
        .collect()
}

#[must_use]
pub fn mem_available() -> u64 {
    meminfo().get("MemAvailable").copied().unwrap_or(0)
}

#[must_use]
pub fn loadavg() -> [f64; 3] {
    let s = read_trim("/proc/loadavg").unwrap_or_default();
    let mut it = s.split_whitespace().map(|v| v.parse().unwrap_or(f64::NAN));
    [
        it.next().unwrap_or(f64::NAN),
        it.next().unwrap_or(f64::NAN),
        it.next().unwrap_or(f64::NAN),
    ]
}

#[must_use]
pub fn top_cpu() -> Vec<String> {
    cmd(
        "ps",
        &["-eo", "pid,pcpu,pmem,comm", "--sort=-pcpu", "--no-headers"],
    )
    .map(|s| s.lines().take(10).map(|l| l.trim().to_owned()).collect())
    .unwrap_or_default()
}

/// Proxmox guests visible from this host (`qm list`, `pct list`); empty when not a PVE host.
#[must_use]
pub fn other_guests() -> Vec<String> {
    let mut out = Vec::new();
    for tool in ["qm", "pct"] {
        if let Some(s) = cmd(tool, &["list"]) {
            out.extend(
                s.lines()
                    .skip(1)
                    .filter(|l| l.contains("running"))
                    .map(|l| format!("{tool}: {}", l.trim())),
            );
        }
    }
    out
}

#[must_use]
pub fn hostname() -> String {
    read_trim("/proc/sys/kernel/hostname")
        .or_else(|| cmd("hostname", &[]))
        .unwrap_or_else(|| "unknown".into())
}

#[must_use]
pub fn git_info() -> GitInfo {
    GitInfo {
        commit: cmd("git", &["rev-parse", "HEAD"]).unwrap_or_default(),
        dirty: cmd("git", &["status", "--porcelain", "--untracked-files=no"])
            .is_some_and(|s| !s.is_empty()),
        branch: cmd("git", &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_default(),
    }
}

/// `(virt, kind)` from `systemd-detect-virt` + the CPU flags.
fn detect_kind(nested_virt: bool) -> (String, String) {
    let virt = cmd("systemd-detect-virt", &[])
        .or_else(|| {
            // exits 1 and prints "none" on bare metal
            Command::new("systemd-detect-virt")
                .output()
                .ok()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".into());
    let kind = match virt.as_str() {
        "none" => "bare-metal",
        "unknown" => "unknown",
        _ if nested_virt => "vm-nested",
        _ => "vm",
    };
    (virt, kind.to_owned())
}

fn cpu_flags(cpuinfo: &str) -> Vec<String> {
    cpuinfo
        .lines()
        .find(|l| l.starts_with("flags"))
        .and_then(|l| l.split_once(':'))
        .map(|(_, f)| f.split_whitespace().map(str::to_owned).collect())
        .unwrap_or_default()
}

fn lscpu() -> BTreeMap<String, String> {
    cmd("lscpu", &[])
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let (k, v) = l.split_once(':')?;
            Some((k.trim().to_owned(), v.trim().to_owned()))
        })
        .collect()
}

fn turbo() -> String {
    if let Some(v) = read_trim("/sys/devices/system/cpu/intel_pstate/no_turbo") {
        return if v == "1" { "off" } else { "on" }.into();
    }
    if let Some(v) = read_trim("/sys/devices/system/cpu/cpufreq/boost") {
        return if v == "1" { "on" } else { "off" }.into();
    }
    "unknown".into()
}

fn ksm() -> Ksm {
    let num = |p: &str| read_trim(p).and_then(|v| v.parse().ok()).unwrap_or(-1);
    Ksm {
        run: num("/sys/kernel/mm/ksm/run"),
        pages_sharing: num("/sys/kernel/mm/ksm/pages_sharing"),
    }
}

fn storage(path: &str) -> Storage {
    let found = cmd("findmnt", &["-no", "SOURCE,FSTYPE", "-T", path]).unwrap_or_default();
    let mut it = found.split_whitespace();
    let device = it.next().unwrap_or_default().to_owned();
    let fs = it.next().unwrap_or_default().to_owned();
    let kind = if device.contains("nvme") {
        "nvme".to_owned()
    } else {
        match cmd("lsblk", &["-ndo", "ROTA", &device]).as_deref() {
            Some("0") => "ssd".into(),
            Some("1") => "hdd".into(),
            _ => "unknown".into(),
        }
    };
    Storage {
        reflink: matches!(fs.as_str(), "btrfs" | "xfs"),
        device,
        kind,
        fs,
        probed_path: path.to_owned(),
    }
}

fn docker_info() -> (DockerInfo, String) {
    let f = |fmt: &str| cmd("docker", &["info", "--format", fmt]).unwrap_or_default();
    let info = DockerInfo {
        engine: cmd("docker", &["version", "--format", "{{.Server.Version}}"]).unwrap_or_default(),
        containerd: f("{{.ContainerdCommit.ID}}"),
        runc: f("{{.RuncCommit.ID}}"),
        cgroup: f("v{{.CgroupVersion}}"),
        storage_driver: f("{{.Driver}}"),
    };
    let root = f("{{.DockerRootDir}}");
    (info, if root.is_empty() { "/".into() } else { root })
}

/// Paths of the Firecracker artifacts the backend would use (if compiled in).
#[derive(Debug, Clone, Default)]
pub struct FcPaths {
    pub firecracker_bin: Option<std::path::PathBuf>,
    pub kernel: Option<std::path::PathBuf>,
}

fn firecracker_info(p: &FcPaths) -> FirecrackerInfo {
    let version = p
        .firecracker_bin
        .as_deref()
        .filter(|b| b.is_file())
        .and_then(|b| cmd(&b.to_string_lossy(), &["--version"]))
        .and_then(|s| s.lines().next().map(str::to_owned))
        .unwrap_or_default();
    let (guest_kernel, guest_kernel_sha256) = match p.kernel.as_deref().filter(|k| k.is_file()) {
        Some(k) => (k.display().to_string(), sha256_file(k).unwrap_or_default()),
        None => (String::new(), String::new()),
    };
    FirecrackerInfo {
        version,
        jailer: false,
        guest_kernel,
        guest_kernel_sha256,
    }
}

/// Collect the full host declaration.
#[must_use]
pub fn collect(kind_override: Option<&str>, crabd: &Path, fc: &FcPaths) -> HostInfo {
    let cpuinfo = fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
    let flags = cpu_flags(&cpuinfo);
    let nested_virt = flags.iter().any(|f| f == "vmx" || f == "svm");
    let (virt, detected_kind) = detect_kind(nested_virt);
    let lscpu = lscpu();
    let num = |k: &str| {
        lscpu
            .get(k)
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(0)
    };
    let sockets = num("Socket(s)");
    let cores_per_socket = num("Core(s) per socket");
    let threads_per_core = num("Thread(s) per core").max(1);
    let threads = num("CPU(s)");
    let mem = meminfo();
    let (docker, docker_root) = docker_info();
    let kvm_available = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/kvm")
        .is_ok();
    HostInfo {
        hostname: hostname(),
        kind: kind_override.map_or(detected_kind, str::to_owned),
        kind_overridden: kind_override.is_some(),
        hypervisor: (virt != "none" && virt != "unknown").then(|| virt.clone()),
        virt,
        cpu_model: lscpu.get("Model name").cloned().unwrap_or_else(|| {
            cpuinfo
                .lines()
                .find(|l| l.starts_with("model name"))
                .and_then(|l| l.split_once(':'))
                .map(|(_, v)| v.trim().to_owned())
                .unwrap_or_default()
        }),
        sockets,
        cores: sockets.max(1) * cores_per_socket,
        threads,
        smt: threads_per_core > 1,
        kernel: cmd("uname", &["-r"]).unwrap_or_default(),
        governor: read_trim("/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor")
            .unwrap_or_else(|| "unavailable (no cpufreq)".into()),
        turbo: turbo(),
        mem_total_bytes: mem.get("MemTotal").copied().unwrap_or(0),
        swap_bytes: mem.get("SwapTotal").copied().unwrap_or(0),
        thp: read_trim("/sys/kernel/mm/transparent_hugepage/enabled").unwrap_or_default(),
        ksm: ksm(),
        kvm_available,
        nested_virt,
        storage: storage(&docker_root),
        docker,
        firecracker: firecracker_info(fc),
        crabd: CrabdInfo {
            path: crabd.display().to_string(),
            sha256: sha256_file(crabd).unwrap_or_default(),
            version: String::new(),
        },
    }
}

#[must_use]
pub fn noise_start(declared: &str, max_load: f64) -> Noise {
    let load = loadavg();
    let mem = meminfo();
    let swap_used = mem
        .get("SwapTotal")
        .copied()
        .unwrap_or(0)
        .saturating_sub(mem.get("SwapFree").copied().unwrap_or(0));
    Noise {
        load_start: load,
        load_end: [0.0; 3],
        other_guests: other_guests(),
        declared: declared.to_owned(),
        top_cpu: top_cpu(),
        top_cpu_end: Vec::new(),
        swap_used_start: swap_used,
        noisy: load[0] > max_load,
        max_load,
    }
}

pub fn noise_end(n: &mut Noise) {
    n.load_end = loadavg();
    n.top_cpu_end = top_cpu();
    if n.load_end[0] > n.max_load {
        n.noisy = true;
    }
}

/// Human-readable host declaration block (`sandcrab-bench host-info`).
#[must_use]
pub fn render(h: &HostInfo, n: &Noise) -> String {
    let gib = |b: u64| b as f64 / (1024.0 * 1024.0 * 1024.0);
    let mut s = String::new();
    let mut line = |k: &str, v: String| {
        s.push_str(&format!("{k:<18} {v}\n"));
    };
    line("hostname", h.hostname.clone());
    line(
        "kind",
        format!(
            "{} (systemd-detect-virt: {}{})",
            h.kind,
            h.virt,
            if h.kind_overridden {
                ", overridden"
            } else {
                ""
            }
        ),
    );
    line(
        "cpu",
        format!(
            "{} — {} socket(s), {} core(s), {} thread(s), smt={}",
            h.cpu_model, h.sockets, h.cores, h.threads, h.smt
        ),
    );
    line("kernel", h.kernel.clone());
    line("governor / turbo", format!("{} / {}", h.governor, h.turbo));
    line(
        "memory",
        format!(
            "{:.1} GiB total, swap {:.1} GiB",
            gib(h.mem_total_bytes),
            gib(h.swap_bytes)
        ),
    );
    line("thp", h.thp.clone());
    line(
        "ksm",
        format!("run={} pages_sharing={}", h.ksm.run, h.ksm.pages_sharing),
    );
    line(
        "kvm / nested",
        format!(
            "/dev/kvm usable={} vmx/svm={}",
            h.kvm_available, h.nested_virt
        ),
    );
    line(
        "storage",
        format!(
            "{} ({}, {}, reflink={}) for {}",
            h.storage.device,
            h.storage.kind,
            h.storage.fs,
            h.storage.reflink,
            h.storage.probed_path
        ),
    );
    line(
        "docker",
        format!(
            "engine {} containerd {} runc {} cgroup {} driver {}",
            h.docker.engine,
            short(&h.docker.containerd),
            short(&h.docker.runc),
            h.docker.cgroup,
            h.docker.storage_driver
        ),
    );
    line(
        "firecracker",
        if h.firecracker.version.is_empty() {
            "not installed".into()
        } else {
            format!(
                "{} kernel {} {}",
                h.firecracker.version,
                h.firecracker.guest_kernel,
                short(&h.firecracker.guest_kernel_sha256)
            )
        },
    );
    line(
        "crabd",
        format!(
            "{} {} {}",
            h.crabd.path,
            short(&h.crabd.sha256),
            h.crabd.version
        ),
    );
    line(
        "load avg",
        format!(
            "{:.2} {:.2} {:.2}{}",
            n.load_start[0],
            n.load_start[1],
            n.load_start[2],
            if n.noisy {
                format!("  NOISY (1-min load > {})", n.max_load)
            } else {
                String::new()
            }
        ),
    );
    line(
        "other guests",
        if n.other_guests.is_empty() {
            "none visible (not a Proxmox host, or none running)".into()
        } else {
            n.other_guests.join("; ")
        },
    );
    line("top cpu", n.top_cpu.first().cloned().unwrap_or_default());
    for p in n.top_cpu.iter().skip(1) {
        line("", p.clone());
    }
    s
}

fn short(s: &str) -> String {
    let s = s.strip_prefix("sha256:").unwrap_or(s);
    s.chars().take(12).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meminfo_parses_kb() {
        let m = parse_meminfo("MemTotal:       8000 kB\nHugePages_Total:       0\n");
        assert_eq!(m["MemTotal"], 8000 * 1024);
        assert_eq!(m["HugePages_Total"], 0);
    }

    #[test]
    fn flags_and_short() {
        let f = cpu_flags("processor: 0\nflags\t\t: fpu vmx sse\n");
        assert!(f.iter().any(|x| x == "vmx"));
        assert_eq!(short("sha256:0123456789abcdef"), "0123456789ab");
    }
}
