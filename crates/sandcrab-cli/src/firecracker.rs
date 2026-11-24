//! Firecracker backend plumbing for the CLI: config, preflight, `doctor` checks, pinned
//! artifact fetch (`doctor --fix`) and `sandcrab rootfs`.
//!
//! Compiled on Linux with the `firecracker` feature (default on) only.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use anyhow::{Context, bail};
use clap::Subcommand;
use sandcrab_sdk::Template;
use sandcrab_sdk::firecracker::artifacts::{self, FIRECRACKER_X86_64, KERNEL_X86_64};
use sandcrab_sdk::firecracker::rootfs::{
    self, MANIFEST_FILE, PIPELINE_VERSION, RootfsBuilder, RootfsManifest, RootfsRequest,
};
use sandcrab_sdk::firecracker::{KVM_DEVICE, hash, reap};
use sandcrab_sdk::{FirecrackerConfig, RootfsSource};
use serde_json::json;

use crate::Global;
use crate::args::BackendArg;
use crate::doctor::{Report, Status};
use crate::util::{self, print_json};

/// Appended to the doctor summary until the backend has passed its end-to-end run on a KVM
/// host (maintainer decision: e2e pending, no KVM host yet).
pub const E2E_NOTE: &str = "e2e pending";

/// Backend config rooted at the CLI state root (`--state-dir` / `$SANDCRAB_HOME`), with the
/// CLI's `crabd` injected into template rootfs images.
pub fn config(g: &Global) -> anyhow::Result<FirecrackerConfig> {
    Ok(FirecrackerConfig::with_home(
        &util::state_root(g)?,
        util::crabd_path(g),
    ))
}

/// CPU virtualization as seen from `/proc/cpuinfo`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Virt {
    /// Running under a hypervisor (`hypervisor` flag).
    guest: bool,
    /// VT-x / AMD-V exposed to this kernel (`vmx` / `svm`).
    hw: bool,
}

fn virt() -> Virt {
    let info = fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
    let flags = info
        .lines()
        .find(|l| l.starts_with("flags"))
        .unwrap_or_default();
    let has = |f: &str| flags.split_whitespace().any(|w| w == f);
    Virt {
        guest: has("hypervisor"),
        hw: has("vmx") || has("svm"),
    }
}

fn virt_label(v: Virt) -> &'static str {
    match (v.guest, v.hw) {
        (false, _) => "bare metal",
        (true, true) => "VM with nested virtualization",
        (true, false) => "VM without nested virtualization",
    }
}

/// Why there is no `/dev/kvm`, and how to get one.
fn no_kvm_hint(v: Virt) -> &'static str {
    match (v.guest, v.hw) {
        (true, false) => {
            "this host is a VM without nested virtualization: enable nested virtualization \
             on the hypervisor (e.g. Proxmox `cpu: host` + `kvm_intel nested=1`) or use a \
             bare-metal host"
        }
        (_, true) => {
            "the CPU exposes VT-x/AMD-V but the kvm module is not loaded: \
             `sudo modprobe kvm_intel` (or `kvm_amd`)"
        }
        (false, false) => "enable VT-x/AMD-V in the firmware (or use a KVM-capable host)",
    }
}

/// Short blocker label + full message when `/dev/kvm` is not usable read-write.
fn kvm_problem() -> Option<(&'static str, String)> {
    let err = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(KVM_DEVICE)
        .err()?;
    Some(match err.kind() {
        std::io::ErrorKind::NotFound => (
            "no /dev/kvm",
            format!(
                "{KVM_DEVICE} not found: {}; then grant access with \
                 `sudo usermod -aG kvm $USER` (and log in again)",
                no_kvm_hint(virt())
            ),
        ),
        std::io::ErrorKind::PermissionDenied => (
            "/dev/kvm not accessible",
            format!(
                "{KVM_DEVICE} is not read-writable by this user ({err}): \
                 `sudo usermod -aG kvm $USER`, then log in again"
            ),
        ),
        _ => ("/dev/kvm unusable", format!("{KVM_DEVICE}: {err}")),
    })
}

/// Fail fast, with the fix, before handing the config to the SDK. Never falls back.
pub fn preflight(c: &FirecrackerConfig) -> anyhow::Result<()> {
    const TAIL: &str = "sandcrab never falls back to another backend; `sandcrab doctor` \
                        lists every Firecracker prerequisite";
    if let Some((_, msg)) = kvm_problem() {
        bail!("firecracker backend unavailable: {msg}. {TAIL}");
    }
    for p in [&c.kernel, &c.firecracker_bin] {
        if !p.is_file() {
            bail!(
                "firecracker backend unavailable: pinned artifact {} is missing; \
                 `sandcrab doctor --fix` downloads and verifies it. {TAIL}",
                p.display()
            );
        }
    }
    if let RootfsSource::Template { crabd } = &c.rootfs
        && !crabd.is_file()
    {
        bail!(
            "firecracker backend unavailable: static crabd {} not found (build it with \
             scripts/build-crabd.sh, or pass --crabd / SANDCRAB_CRABD). {TAIL}",
            crabd.display()
        );
    }
    Ok(())
}

/// Supplementary groups of this process, by name (`id -Gn`).
fn group_names() -> Vec<String> {
    Command::new("id")
        .arg("-Gn")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .split_whitespace()
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// Real uid from `/proc/self/status`.
fn uid() -> Option<i64> {
    fs::read_to_string("/proc/self/status")
        .ok()?
        .lines()
        .find_map(|l| l.strip_prefix("Uid:"))?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

/// Check a pinned artifact; true when present and its SHA-256 matches.
fn artifact(
    r: &mut Report,
    name: &str,
    path: &Path,
    version: &str,
    sha: &str,
    bad: Status,
) -> bool {
    if !path.is_file() {
        r.add(
            name,
            bad,
            format!(
                "{} missing; `sandcrab doctor --fix` downloads + verifies it",
                path.display()
            ),
        );
        return false;
    }
    match hash::verify_sha256(path, sha) {
        Ok(()) => {
            r.add(
                name,
                Status::Ok,
                format!("{version} at {} (sha256 pin verified)", path.display()),
            );
            true
        }
        Err(e) => {
            // A pinned artifact that does not match is never used: always a failure.
            r.add(
                name,
                Status::Fail,
                format!("{e}; delete it and run `sandcrab doctor --fix`"),
            );
            false
        }
    }
}

/// The template's image id: its host-arch pin, else the local Docker image id.
fn image_id(t: &Template) -> Option<String> {
    t.manifest
        .pinned_sha256_for_host()
        .map(str::to_owned)
        .or_else(|| rootfs::docker_image_id(t.image()).ok())
}

fn rootfs_cache(r: &mut Report, g: &Global, cache_root: &Path, crabd_sha: Option<&str>) {
    let templates = match util::templates(g) {
        Ok(t) => t.list(),
        Err(e) => return r.add("fc:rootfs", Status::Warn, e.to_string()),
    };
    // Firecracker guests are host-arch.
    let arch = match rootfs::Arch::host() {
        Ok(a) => a,
        Err(e) => return r.add("fc:rootfs", Status::Warn, e.to_string()),
    };
    let builder = RootfsBuilder::new(cache_root);
    for t in &templates {
        let name = format!("fc:rootfs:{}", t.name());
        let Some(crabd_sha) = crabd_sha else {
            r.add(name, Status::Info, "unknown (needs a valid crabd)");
            continue;
        };
        let Some(id) = image_id(t) else {
            r.add(
                name,
                Status::Info,
                format!(
                    "{} unpinned and not built; `sandcrab template build {}` first",
                    t.image(),
                    t.name()
                ),
            );
            continue;
        };
        let key = rootfs::cache_key(&id, arch, crabd_sha, PIPELINE_VERSION);
        match builder.lookup(&key) {
            Ok(Some((dir, m))) => r.add(
                name,
                Status::Ok,
                format!(
                    "cached ({} MiB, mkfs={:?}) at {}",
                    m.size_bytes >> 20,
                    m.mkfs,
                    dir.display()
                ),
            ),
            Ok(None) => r.add(
                name,
                Status::Info,
                format!(
                    "not built for this crabd (built on first spawn, or now with \
                     `sandcrab rootfs build {}`)",
                    t.name()
                ),
            ),
            Err(e) => r.add(name, Status::Warn, e.to_string()),
        }
    }
}

fn taps(r: &mut Report, c: &FirecrackerConfig) {
    let pool = &c.taps;
    let me = uid();
    let (mut present, mut mine) = (0u32, 0u32);
    for i in 0..pool.size {
        let dir = pool.sysfs_net.join(format!("{}{i}", pool.prefix));
        if !dir.exists() {
            continue;
        }
        present += 1;
        let owner = fs::read_to_string(dir.join("owner"))
            .ok()
            .and_then(|s| s.trim().parse::<i64>().ok());
        if owner.is_some() && owner == me {
            mine += 1;
        }
    }
    let setup = format!(
        "`sudo scripts/fc-net-setup.sh --user $USER --count {}` (nft table `inet sandcrab`)",
        pool.size
    );
    if present == 0 {
        r.add(
            "fc:taps",
            Status::Info,
            format!(
                "no {}N tap pool: `--network disabled` only; for `unrestricted` run {setup}",
                pool.prefix
            ),
        );
    } else if mine == 0 {
        r.add(
            "fc:taps",
            Status::Warn,
            format!(
                "{present} {}N tap(s), none owned by this user; re-run {setup}",
                pool.prefix
            ),
        );
    } else {
        r.add(
            "fc:taps",
            Status::Ok,
            format!(
                "{mine}/{} {}N tap(s) owned by this user (unrestricted networking)",
                pool.size, pool.prefix
            ),
        );
    }
}

/// `doctor` Firecracker section. Problems are failures only when firecracker is the
/// selected backend (`--backend firecracker`); otherwise warnings.
pub fn doctor(g: &Global, r: &mut Report, fix: bool) {
    let selected = matches!(g.backend, Some(BackendArg::Firecracker));
    let bad = if selected { Status::Fail } else { Status::Warn };
    let c = match config(g) {
        Ok(c) => c,
        Err(e) => return r.add("firecracker", bad, e.to_string()),
    };
    let mut blockers: Vec<&str> = Vec::new();

    // KVM + group.
    let v = virt();
    let kvm = kvm_problem();
    match &kvm {
        None => r.add(
            "fc:kvm",
            Status::Ok,
            format!("{KVM_DEVICE} read-write ({})", virt_label(v)),
        ),
        Some((short, msg)) => {
            blockers.push(short);
            r.add("fc:kvm", bad, msg.clone());
        }
    }
    let in_kvm = group_names().iter().any(|n| n == "kvm");
    match (in_kvm, &kvm) {
        (true, _) => r.add("fc:kvm_group", Status::Ok, "member of group `kvm`"),
        (false, None) => r.add(
            "fc:kvm_group",
            Status::Info,
            "not in group `kvm`, but /dev/kvm is accessible (ACL)",
        ),
        (false, Some(_)) => r.add(
            "fc:kvm_group",
            Status::Info,
            "not in group `kvm`: `sudo usermod -aG kvm $USER` once /dev/kvm exists",
        ),
    }

    // Pinned artifacts (fetched first with --fix).
    if std::env::consts::ARCH == "x86_64" {
        if fix {
            let fetched = artifacts::fetch_kernel(&c.cache_root)
                .and_then(|_| artifacts::fetch_firecracker(&c.cache_root, false).map(|_| ()));
            match fetched {
                Ok(()) => r.add(
                    "fc:fetch",
                    Status::Ok,
                    format!("pinned kernel + firecracker in {}", c.cache_root.display()),
                ),
                Err(e) => r.add("fc:fetch", Status::Fail, e.to_string()),
            }
        }
        let fc_ok = artifact(
            r,
            "fc:firecracker",
            &c.firecracker_bin,
            &format!("firecracker v{}", FIRECRACKER_X86_64.version),
            FIRECRACKER_X86_64.firecracker_sha256,
            bad,
        );
        let kernel_ok = artifact(
            r,
            "fc:kernel",
            &c.kernel,
            &format!("kernel {}", KERNEL_X86_64.version),
            KERNEL_X86_64.sha256,
            bad,
        );
        if !(fc_ok && kernel_ok) {
            blockers.push("artifacts missing");
        }
    } else {
        r.add(
            "fc:artifacts",
            bad,
            format!(
                "no pinned Firecracker artifacts for {} (x86_64 only in V1)",
                std::env::consts::ARCH
            ),
        );
        blockers.push("unsupported arch");
    }

    // mke2fs strategy for rootfs / workspace images.
    match rootfs::mkfs::detect(&std::env::temp_dir()) {
        Ok(rootfs::MkfsStrategy::NativeTar) => {
            r.add("fc:mkfs", Status::Ok, "native (e2fsprogs >= 1.47.1)");
        }
        Ok(s) => r.add(
            "fc:mkfs",
            Status::Info,
            format!("{s:?} fallback (host e2fsprogs < 1.47.1 or without libarchive)"),
        ),
        Err(e) => {
            blockers.push("no mke2fs");
            r.add("fc:mkfs", bad, e.to_string());
        }
    }

    // crabd + rootfs cache per template.
    let crabd = util::crabd_path(g);
    let crabd_sha = if crabd.is_file() {
        hash::sha256_file(&crabd).ok()
    } else {
        None
    };
    match &crabd_sha {
        Some(sha) => r.add(
            "fc:crabd",
            Status::Ok,
            format!("{} (sha256 {})", crabd.display(), &sha[..12]),
        ),
        None => {
            blockers.push("no crabd");
            r.add(
                "fc:crabd",
                bad,
                format!(
                    "{} not found (scripts/build-crabd.sh, --crabd or SANDCRAB_CRABD)",
                    crabd.display()
                ),
            );
        }
    }
    rootfs_cache(r, g, &c.cache_root, crabd_sha.as_deref());

    taps(r, &c);

    if fix {
        match reap::reap_run_dir(&c.run_dir, &c.recovery_dir) {
            Ok(rep) => r.add(
                "fc:orphans",
                Status::Ok,
                format!(
                    "reaped {} run dir(s), killed {} VMM(s), recovered {} workspace image(s)",
                    rep.run_dirs.len(),
                    rep.killed.len(),
                    rep.recovered.len()
                ),
            ),
            Err(e) => r.add("fc:orphans", Status::Fail, e.to_string()),
        }
    }

    if blockers.is_empty() {
        r.add(
            "firecracker",
            Status::Ok,
            format!("Firecracker: ready (hardware isolated) — {E2E_NOTE}"),
        );
    } else {
        r.add(
            "firecracker",
            if selected { Status::Fail } else { Status::Info },
            format!(
                "Firecracker: unavailable ({}) — {E2E_NOTE}",
                blockers.join(", ")
            ),
        );
    }
}

#[derive(Debug, Subcommand)]
pub enum RootfsCmd {
    /// Build (or reuse) the read-only ext4 rootfs of templates for the Firecracker
    /// backend: template image → export → inject crabd → mke2fs. Needs Docker and the
    /// built template image (`sandcrab template build`); no KVM needed.
    Build {
        /// Template names.
        #[arg(required_unless_present = "all", conflicts_with = "all")]
        names: Vec<String>,
        /// Every template.
        #[arg(long)]
        all: bool,
        /// Re-hash a cached image against its manifest.
        #[arg(long)]
        verify: bool,
    },
    /// List cached rootfs images.
    #[command(alias = "ls")]
    List,
}

fn cached_manifests(rootfs_dir: &Path) -> Vec<(PathBuf, RootfsManifest)> {
    let Ok(entries) = fs::read_dir(rootfs_dir) else {
        return Vec::new();
    };
    let mut all: Vec<_> = entries
        .flatten()
        .filter_map(|e| {
            let dir = e.path();
            RootfsManifest::load(&dir.join(MANIFEST_FILE))
                .ok()
                .map(|m| (dir, m))
        })
        .collect();
    all.sort_by_key(|(_, m)| std::cmp::Reverse(m.created_at));
    all
}

pub fn rootfs_cmd(g: &Global, cmd: RootfsCmd) -> anyhow::Result<u8> {
    let c = config(g)?;
    match cmd {
        RootfsCmd::List => {
            let all = cached_manifests(RootfsBuilder::new(&c.cache_root).rootfs_dir());
            if g.json {
                print_json(&all.iter().map(|(_, m)| m).collect::<Vec<_>>())?;
            } else if all.is_empty() {
                println!(
                    "no cached rootfs images (build one with `sandcrab rootfs build <template>`)"
                );
            } else {
                println!(
                    "{:<12} {:<26} {:>7} {:<14} DIR",
                    "TEMPLATE", "IMAGE", "MIB", "CRABD"
                );
                for (dir, m) in &all {
                    println!(
                        "{:<12} {:<26} {:>7} {:<14} {}",
                        m.template.as_deref().unwrap_or("-"),
                        m.image,
                        m.size_bytes >> 20,
                        &m.crabd_sha256[..m.crabd_sha256.len().min(12)],
                        dir.display()
                    );
                }
            }
            Ok(0)
        }
        RootfsCmd::Build { names, all, verify } => {
            let crabd = util::crabd_path(g);
            if !crabd.is_file() {
                bail!(
                    "static crabd {} not found (build it with scripts/build-crabd.sh, or pass \
                     --crabd / SANDCRAB_CRABD)",
                    crabd.display()
                );
            }
            let templates = util::templates(g)?;
            let selected: Vec<Template> = if all {
                templates.list()
            } else {
                names
                    .iter()
                    .map(|n| templates.get(n))
                    .collect::<Result<_, _>>()?
            };
            if selected.is_empty() {
                bail!("no templates found (pass --templates-dir or run from the repo root)");
            }
            let builder = RootfsBuilder::new(&c.cache_root).verify_on_hit(verify);
            // Firecracker guests are host-arch.
            let arch = rootfs::Arch::host()?;
            let mut out = Vec::new();
            for t in &selected {
                let pin = t.manifest.pinned_sha256_for_host().map(str::to_owned);
                if pin.is_none() {
                    eprintln!(
                        "warning: template `{}` is not pinned; the rootfs is keyed by the \
                         local image id (`sandcrab template build {}` pins it)",
                        t.name(),
                        t.name()
                    );
                }
                if !g.json {
                    eprintln!("sandcrab: building rootfs for `{}` ...", t.name());
                }
                let t0 = Instant::now();
                let built = builder
                    .build(&RootfsRequest {
                        image: t.image().to_owned(),
                        expected_image_id: pin,
                        template: Some(t.name().to_owned()),
                        crabd: crabd.clone(),
                        arch,
                    })
                    .with_context(|| {
                        format!(
                            "building the rootfs of `{}` (image {}; is it built? \
                             `sandcrab template build {}`)",
                            t.name(),
                            t.image(),
                            t.name()
                        )
                    })?;
                let m = &built.manifest;
                if g.json {
                    out.push(json!({
                        "template": t.name(),
                        "path": built.image_path,
                        "cache_hit": built.cache_hit,
                        "cache_key": m.cache_key,
                        "image_id": m.image_id,
                        "arch": m.arch,
                        "crabd_sha256": m.crabd_sha256,
                        "rootfs_sha256": m.rootfs_sha256,
                        "size_bytes": m.size_bytes,
                        "mkfs": m.mkfs,
                        "elapsed_ms": t0.elapsed().as_millis(),
                    }));
                } else {
                    println!(
                        "{} `{}`: {} ({} MiB, mkfs={:?}, {:.1?})",
                        if built.cache_hit { "cached" } else { "built" },
                        t.name(),
                        built.image_path.display(),
                        m.size_bytes >> 20,
                        m.mkfs,
                        t0.elapsed()
                    );
                }
            }
            if g.json {
                print_json(&out)?;
            }
            Ok(0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn virt_labels_and_hints() {
        let vm = Virt {
            guest: true,
            hw: false,
        };
        assert_eq!(virt_label(vm), "VM without nested virtualization");
        assert!(no_kvm_hint(vm).contains("nested virtualization"));
        let nested = Virt {
            guest: true,
            hw: true,
        };
        assert!(no_kvm_hint(nested).contains("modprobe"));
        assert_eq!(
            virt_label(Virt {
                guest: false,
                hw: true
            }),
            "bare metal"
        );
    }

    #[test]
    fn preflight_names_the_fix() {
        let tmp = tempfile::tempdir().unwrap();
        let c = FirecrackerConfig::with_home(tmp.path(), tmp.path().join("crabd"));
        let err = preflight(&c).unwrap_err().to_string();
        assert!(err.contains("never falls back"), "{err}");
        if kvm_problem().is_some() {
            assert!(err.contains("/dev/kvm"), "{err}");
            assert!(
                err.contains("usermod -aG kvm")
                    || err.contains("nested virtualization")
                    || err.contains("modprobe")
                    || err.contains("VT-x"),
                "{err}"
            );
        } else {
            assert!(err.contains("doctor --fix"), "{err}");
        }
    }
}
