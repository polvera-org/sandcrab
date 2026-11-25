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
    #[cfg(target_os = "macos")]
    {
        macos::meminfo()
    }
    #[cfg(not(target_os = "macos"))]
    {
        parse_meminfo(&fs::read_to_string("/proc/meminfo").unwrap_or_default())
    }
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
    #[cfg(target_os = "macos")]
    let s = macos::sysctl("vm.loadavg")
        .unwrap_or_default()
        .trim_matches(|c| c == '{' || c == '}' || char::is_whitespace(c))
        .to_owned();
    #[cfg(not(target_os = "macos"))]
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
    #[cfg(target_os = "macos")]
    {
        macos::top_cpu()
    }
    #[cfg(not(target_os = "macos"))]
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

/// Replace the recorded hostname with `label` (`--host-label`). The hostname is the only
/// host-identifying name [`HostInfo`] carries, so after this the label is the only name in
/// the result.
pub fn apply_label(info: &mut HostInfo, label: Option<&str>) {
    if let Some(l) = label {
        info.hostname = l.to_owned();
    }
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
        match cmd("lsblk", &["-ndo", "ROTA", &device])
            .or_else(|| sys_rotational(&device))
            .as_deref()
        {
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

/// `/sys/block/<dev>/queue/rotational` for a device node (`/dev/md3`, `/dev/sda1`): a
/// partition reads its parent disk's queue. Fallback when `lsblk` is missing. A
/// `/dev/mapper/*` link resolves to its `/dev/dm-N` node first.
fn sys_rotational(device: &str) -> Option<String> {
    let real = fs::canonicalize(device).ok();
    let name = real
        .as_deref()
        .and_then(|p| p.to_str())
        .unwrap_or(device)
        .strip_prefix("/dev/")?;
    read_trim(format!("/sys/block/{name}/queue/rotational")).or_else(|| {
        let real = fs::canonicalize(format!("/sys/class/block/{name}")).ok()?;
        read_trim(real.parent()?.join("queue/rotational"))
    })
}

/// Storage backing `path` (device, `nvme`/`ssd`/`hdd`/`unknown`, filesystem). Linux: `findmnt`
/// plus the rotational flag; macOS: `df` plus `diskutil` (`Solid State: Yes` on Apple Silicon).
#[must_use]
pub fn storage_for(path: &Path) -> Storage {
    #[cfg(target_os = "macos")]
    {
        macos::storage_at(path)
    }
    #[cfg(not(target_os = "macos"))]
    {
        storage(&path.to_string_lossy())
    }
}

/// `(free bytes, mount point)` of the filesystem holding `path`, from `df -Pk`.
#[must_use]
pub fn disk_free(path: &Path) -> Option<(u64, String)> {
    let out = cmd("df", &["-Pk", &path.to_string_lossy()])?;
    parse_df(&out)
}

fn parse_df(out: &str) -> Option<(u64, String)> {
    let line = out.lines().nth(1)?;
    let cols: Vec<&str> = line.split_whitespace().collect();
    let avail_kb: u64 = cols.get(3)?.parse().ok()?;
    Some((avail_kb * 1024, (*cols.last()?).to_owned()))
}

/// The Sandcrab home (`$SANDCRAB_HOME`, else `~/.sandcrab`): pinned VMM artifacts and the
/// shared rootfs cache live here.
#[must_use]
pub fn sandcrab_home() -> Option<std::path::PathBuf> {
    std::env::var_os("SANDCRAB_HOME")
        .filter(|v| !v.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| Path::new(&h).join(".sandcrab")))
}

/// Docker's root dir, if the daemon answers and the dir is visible on this host (it is not
/// with Docker Desktop, whose root lives inside its VM).
#[must_use]
pub fn docker_root() -> Option<std::path::PathBuf> {
    let root = cmd("docker", &["info", "--format", "{{.DockerRootDir}}"])?;
    let p = std::path::PathBuf::from(root);
    p.is_dir().then_some(p)
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
    #[allow(unused_mut)]
    let mut info = HostInfo {
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
    };
    #[cfg(target_os = "macos")]
    macos::fill_host(&mut info);
    info
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

/// macOS equivalents of the `/proc`/`/sys` probes (no procfs there). Same contract: read-only,
/// best-effort, `unknown`/empty when a value cannot be read. Commands run with `LC_ALL=C`
/// (sysctl and vm_stat otherwise print the locale's decimal comma).
#[cfg(target_os = "macos")]
pub(crate) mod macos {
    use std::collections::BTreeMap;
    use std::process::Command;

    use crate::results::{HostInfo, Storage};

    fn cmd_c(program: &str, args: &[&str]) -> Option<String> {
        let out = Command::new(program)
            .args(args)
            .env("LC_ALL", "C")
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
    }

    pub fn sysctl(name: &str) -> Option<String> {
        cmd_c("sysctl", &["-n", name])
    }

    fn sysctl_u64(name: &str) -> Option<u64> {
        sysctl(name)?.parse().ok()
    }

    /// A `/proc/meminfo`-shaped map (bytes) with the keys the bench reads: `MemTotal`
    /// (`hw.memsize`), `MemAvailable` (free + file-backed + purgeable pages from `vm_stat`, an
    /// approximation: macOS has no MemAvailable), `SwapTotal`/`SwapFree` (`vm.swapusage`).
    pub fn meminfo() -> BTreeMap<String, u64> {
        let mut m = BTreeMap::new();
        if let Some(t) = sysctl_u64("hw.memsize") {
            m.insert("MemTotal".into(), t);
        }
        if let Some(a) = cmd_c("vm_stat", &[])
            .as_deref()
            .and_then(parse_vm_stat_available)
        {
            m.insert("MemAvailable".into(), a);
        }
        if let Some((total, free)) = sysctl("vm.swapusage").as_deref().and_then(parse_swapusage) {
            m.insert("SwapTotal".into(), total);
            m.insert("SwapFree".into(), free);
        }
        m
    }

    pub(crate) fn parse_vm_stat_available(text: &str) -> Option<u64> {
        let page: u64 = text
            .lines()
            .next()?
            .split("page size of ")
            .nth(1)?
            .split_whitespace()
            .next()?
            .parse()
            .ok()?;
        let get = |key: &str| -> Option<u64> {
            text.lines()
                .find_map(|l| l.strip_prefix(key))?
                .trim()
                .trim_end_matches('.')
                .parse()
                .ok()
        };
        // Linux MemAvailable ~ free + reclaimable page cache: free + file-backed + purgeable.
        // Older vm_stat without the file-backed line: free + inactive + speculative.
        let pages = match get("File-backed pages:") {
            Some(file) => get("Pages free:")? + file + get("Pages purgeable:").unwrap_or(0),
            None => get("Pages free:")? + get("Pages inactive:")? + get("Pages speculative:")?,
        };
        Some(pages * page)
    }

    /// `total = 2048.00M  used = 1213.25M  free = 834.75M  (encrypted)` → (total, free) bytes.
    pub(crate) fn parse_swapusage(text: &str) -> Option<(u64, u64)> {
        let field = |key: &str| -> Option<u64> {
            let v = text.split(key).nth(1)?.split_whitespace().next()?;
            let (num, unit) = v.split_at(v.len().checked_sub(1)?);
            let mult = match unit {
                "K" => 1u64 << 10,
                "M" => 1 << 20,
                "G" => 1 << 30,
                _ => return None,
            };
            Some((num.replace(',', ".").parse::<f64>().ok()? * mult as f64) as u64)
        };
        Some((field("total = ")?, field("free = ")?))
    }

    /// Top 10 processes by CPU as `pid pcpu pmem name`. Only the executable's base name is
    /// kept: macOS `comm` is the full path, which can carry user names and home directories.
    pub fn top_cpu() -> Vec<String> {
        cmd_c("ps", &["-Aro", "pid,pcpu,pmem,comm"])
            .map(|s| {
                s.lines()
                    .skip(1)
                    .take(10)
                    .filter_map(|l| {
                        let mut it = l.split_whitespace();
                        let (pid, cpu, mem) = (it.next()?, it.next()?, it.next()?);
                        let comm = it.collect::<Vec<_>>().join(" ");
                        let base = comm.rsplit('/').next().unwrap_or(&comm);
                        Some(format!("{pid} {cpu} {mem} {base}"))
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// CPU, kind and storage for a Mac. The storage probe is the volume holding the Sandcrab
    /// state dir (rootfs cache, scratch drives), recorded by mount point, never by home path.
    pub fn fill_host(h: &mut HostInfo) {
        h.cpu_model = sysctl("machdep.cpu.brand_string").unwrap_or_default();
        h.sockets = sysctl_u64("hw.packages").map_or(1, |v| v as u32);
        h.cores = sysctl_u64("hw.physicalcpu").unwrap_or(0) as u32;
        h.threads = sysctl_u64("hw.logicalcpu").unwrap_or(0) as u32;
        h.smt = h.threads > h.cores;
        let in_vm = sysctl_u64("kern.hv_vmm_present") == Some(1);
        if !h.kind_overridden {
            h.kind = if in_vm { "vm" } else { "bare-metal" }.into();
        }
        h.virt = if in_vm { "apple-vm" } else { "none" }.into();
        h.hypervisor = in_vm.then(|| "apple-vm".into());
        h.kernel = format!(
            "Darwin {} (macOS {})",
            sysctl("kern.osrelease").unwrap_or_default(),
            cmd_c("sw_vers", &["-productVersion"]).unwrap_or_default()
        );
        h.governor = "n/a (macOS)".into();
        h.thp = "n/a (macOS)".into();
        h.storage = storage();
    }

    fn storage() -> Storage {
        let state = super::sandcrab_home()
            .filter(|p| p.exists())
            .unwrap_or_else(|| "/".into());
        storage_at(&state)
    }

    /// Storage of the volume holding `path`, recorded by device and mount point only.
    pub fn storage_at(path: &std::path::Path) -> Storage {
        let df = cmd_c("df", &[&path.to_string_lossy()]).unwrap_or_default();
        let line = df.lines().nth(1).unwrap_or_default();
        let device = line
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .to_owned();
        let mount = line
            .split_whitespace()
            .last()
            .unwrap_or_default()
            .to_owned();
        let info = cmd_c("diskutil", &["info", &mount]).unwrap_or_default();
        let field = |k: &str| {
            info.lines()
                .find_map(|l| l.trim().strip_prefix(k))
                .map(|v| v.trim().to_owned())
                .unwrap_or_default()
        };
        let ssd = field("Solid State:");
        let location = field("Device Location:");
        let kind = match ssd.as_str() {
            "Yes" if location == "Internal" => "ssd (internal)".to_owned(),
            "Yes" => "ssd".to_owned(),
            "No" => "hdd".to_owned(),
            _ => "unknown".to_owned(),
        };
        let fs = field("File System Personality:");
        Storage {
            reflink: fs.eq_ignore_ascii_case("apfs"),
            device,
            kind,
            fs,
            probed_path: mount,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_label_is_the_only_host_name_in_the_output() {
        let real = "real-box-0042.internal.example";
        let mut info = HostInfo {
            hostname: real.into(),
            kind: "bare-metal".into(),
            ..HostInfo::default()
        };
        apply_label(&mut info, Some("kvm-dedicated"));
        let json = serde_json::to_string(&info).unwrap();
        assert!(json.contains("\"hostname\":\"kvm-dedicated\""), "{json}");
        assert!(!json.contains(real), "{json}");
        assert!(!json.contains("real-box"), "{json}");
        // No label: the real hostname is kept.
        let mut info = HostInfo {
            hostname: real.into(),
            ..HostInfo::default()
        };
        apply_label(&mut info, None);
        assert_eq!(info.hostname, real);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_vm_stat_and_swapusage_parse() {
        let vm = "Mach Virtual Memory Statistics: (page size of 16384 bytes)\n\
                  Pages free:                                4056.\n\
                  Pages active:                            266890.\n\
                  Pages inactive:                          263195.\n\
                  Pages speculative:                         2805.\n";
        assert_eq!(
            macos::parse_vm_stat_available(vm),
            Some((4056 + 263_195 + 2805) * 16384)
        );
        let vm2 = format!(
            "{vm}Pages purgeable:                            770.\n\
             File-backed pages:                       186655.\n"
        );
        assert_eq!(
            macos::parse_vm_stat_available(&vm2),
            Some((4056 + 186_655 + 770) * 16384)
        );
        assert_eq!(
            macos::parse_swapusage(
                "total = 2048.00M  used = 1024.00M  free = 1024.00M  (encrypted)"
            ),
            Some((2048 << 20, 1024 << 20))
        );
        assert_eq!(
            macos::parse_swapusage("total = 0.00M  used = 0.00M  free = 0.00M  (encrypted)"),
            Some((0, 0))
        );
    }

    #[test]
    fn df_parses_free_and_mount() {
        let out = "Filesystem 1024-blocks Used Available Capacity Mounted on\n\
                   /dev/md3 100 40 60 40% /\n";
        assert_eq!(parse_df(out), Some((60 * 1024, "/".into())));
        assert_eq!(parse_df("header only"), None);
    }

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
