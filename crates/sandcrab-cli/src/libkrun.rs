//! libkrun backend plumbing for the CLI: config, preflight, `doctor` checks and
//! `sandcrab libkrun install` (pinned libkrun + libkrunfw, pinned userspace netstack, signed
//! `sandcrab-krun` helper).
//!
//! Compiled with the `libkrun` feature (default on) on macOS aarch64 and Linux only.

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, bail};
use clap::Subcommand;
use sandcrab_sdk::libkrun::artifacts::{self, Artifacts, ResolvedArtifacts};
use sandcrab_sdk::libkrun::config::HELPER_BIN_NAME;
#[cfg(target_os = "macos")]
use sandcrab_sdk::libkrun::config::{
    ENTITLEMENTS_HARDENED_PLIST, ENTITLEMENTS_PLIST, HYPERVISOR_ENTITLEMENT, SIGN_RUNTIME_ENV,
};
use sandcrab_sdk::libkrun::verified::HelperRecord;
use sandcrab_sdk::libkrun::{RootfsSource, netstack_pins, reap};
use sandcrab_sdk::{LibkrunConfig, WorkspaceBacking};
use serde_json::json;

use crate::Global;
use crate::args::BackendArg;
use crate::doctor::{Report, Status};
use crate::util::{self, print_json};

/// `$SANDCRAB_SIGN_IDENTITY`: codesign identity for the helper (default `-`, ad hoc).
#[cfg(target_os = "macos")]
pub const SIGN_IDENTITY_ENV: &str = "SANDCRAB_SIGN_IDENTITY";

/// The state root, made absolute (the backend wants absolute library and socket paths).
fn home(g: &Global) -> anyhow::Result<PathBuf> {
    let root = util::state_root(g)?;
    std::path::absolute(&root).with_context(|| format!("resolving {}", root.display()))
}

/// Backend config rooted at the CLI state root (`--state-dir` / `$SANDCRAB_HOME`): helper in
/// `<root>/bin/`, libraries from `<root>/artifacts/libkrun/<version>/` (where `sandcrab
/// libkrun install` puts them), the CLI's `crabd` injected into template rootfs images.
pub fn config(g: &Global) -> anyhow::Result<LibkrunConfig> {
    let home = home(g)?;
    let mut c = LibkrunConfig::with_home(&home, std::path::absolute(util::crabd_path(g))?);
    c.lib_dir = Some(artifacts::default_dir(&home));
    c.allow_tsi_unrestricted = g.libkrun_tsi;
    c.cgroup_parent = sandcrab_sdk::libkrun::config::cgroup_parent_from_env();
    Ok(c)
}

/// Hypervisor availability: `kern.hv_support` (macOS) / a read-writable `/dev/kvm` (Linux).
#[cfg(target_os = "macos")]
fn hypervisor() -> Result<String, String> {
    let out = Command::new("/usr/sbin/sysctl")
        .args(["-n", "kern.hv_support"])
        .output()
        .map_err(|e| format!("cannot run sysctl: {e}"))?;
    let v = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    if out.status.success() && v == "1" {
        Ok("kern.hv_support = 1 (Hypervisor.framework available)".into())
    } else {
        Err(format!(
            "kern.hv_support = {:?}: Hypervisor.framework is unavailable (a VM without nested \
             virtualization, or unsupported hardware)",
            if v.is_empty() { "?" } else { v.as_str() }
        ))
    }
}

#[cfg(target_os = "linux")]
fn hypervisor() -> Result<String, String> {
    const KVM: &str = "/dev/kvm";
    match fs::OpenOptions::new().read(true).write(true).open(KVM) {
        Ok(_) => Ok(format!("{KVM} read-write")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(format!(
            "{KVM} not found: this host has no KVM (use bare metal, or a VM with nested \
             virtualization)"
        )),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => Err(format!(
            "{KVM} is not read-writable by this user ({e}): `sudo usermod -aG kvm $USER`, then \
             log in again"
        )),
        Err(e) => Err(format!("{KVM}: {e}")),
    }
}

/// Does `bin` carry `com.apple.security.hypervisor`?
#[cfg(target_os = "macos")]
fn has_entitlement(bin: &Path) -> bool {
    Command::new("/usr/bin/codesign")
        .args(["-d", "--entitlements", "-", "--xml"])
        .arg(bin)
        .stdin(std::process::Stdio::null())
        .output()
        .is_ok_and(|o| {
            o.status.success()
                && String::from_utf8_lossy(&o.stdout).contains(HYPERVISOR_ENTITLEMENT)
        })
}

/// The helper exists, is executable and (macOS) carries the hypervisor entitlement.
fn helper_check(helper: &Path) -> Result<String, String> {
    let exec =
        fs::metadata(helper).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0);
    if !exec {
        return Err(format!(
            "{} missing or not executable; `sandcrab libkrun install` installs and signs it",
            helper.display()
        ));
    }
    #[cfg(target_os = "macos")]
    if !has_entitlement(helper) {
        return Err(format!(
            "{} lacks {HYPERVISOR_ENTITLEMENT}; re-run `sandcrab libkrun install` (or \
             scripts/sign-krun-helper.sh {})",
            helper.display(),
            helper.display()
        ));
    }
    // The install record (sha256 + stat stamp) the backend checks before every launch.
    let record = match HelperRecord::load(helper).map_err(|e| e.to_string())? {
        Some(r) => match sandcrab_sdk::libkrun::verified::open_pinned(&r.pinned(helper)) {
            Ok(_) => format!("install record ok, sha256 {}", short(&r.sha256)),
            Err(e) => {
                return Err(format!(
                    "{} does not match its install record ({e}); re-run `sandcrab libkrun \
                     install` if you replaced it on purpose",
                    helper.display()
                ));
            }
        },
        None => "no install record: trusted on first use per process; `sandcrab libkrun \
                 install` records it"
            .to_owned(),
    };
    #[cfg(target_os = "macos")]
    return Ok(format!(
        "{} (signed, {HYPERVISOR_ENTITLEMENT}; {record})",
        helper.display()
    ));
    #[cfg(not(target_os = "macos"))]
    Ok(format!("{} (executable; {record})", helper.display()))
}

fn short(sha: &str) -> &str {
    &sha[..sha.len().min(12)]
}

fn describe(a: &ResolvedArtifacts) -> String {
    let origin = |m: Option<&artifacts::LibManifest>| {
        m.map_or_else(|| "unverified".to_owned(), |m| m.origin.as_str().to_owned())
    };
    format!(
        "{}: libkrun {} sha256 {} ({}), libkrunfw {} sha256 {} ({}); pins verified",
        a.dir.display(),
        artifacts::LIBKRUN_VERSION,
        short(&a.libkrun.sha256),
        origin(a.manifest.as_ref().map(|m| &m.libkrun)),
        artifacts::LIBKRUNFW_VERSION,
        short(&a.libkrunfw.sha256),
        origin(a.manifest.as_ref().map(|m| &m.libkrunfw)),
    )
}

/// Is the volume holding `dir` case-insensitive? Read-only probe: stat the nearest existing
/// path component that has cased letters under its case-swapped name and compare inodes.
/// `None` when it cannot tell.
pub fn case_insensitive(dir: &Path) -> Option<bool> {
    let mut p = dir;
    loop {
        if let (Ok(md), Some(name)) = (fs::metadata(p), p.file_name().and_then(|n| n.to_str())) {
            let swapped: String = name
                .chars()
                .map(|c| {
                    if c.is_uppercase() {
                        c.to_ascii_lowercase()
                    } else {
                        c.to_ascii_uppercase()
                    }
                })
                .collect();
            if swapped != name {
                return Some(
                    fs::metadata(p.with_file_name(swapped))
                        .is_ok_and(|m| m.dev() == md.dev() && m.ino() == md.ino()),
                );
            }
        }
        p = p.parent()?;
    }
}

/// Warn about case-insensitive workspace roots: virtio-fs shares them live, so `Readme` and
/// `readme` collide inside the guest (design §8).
fn workspace_case(g: &Global, r: &mut Report, selected: bool) {
    let Ok(root) = home(g) else { return };
    let mut dirs = vec![root.join("workspaces")];
    // Only read an existing registry (doctor must not create the state dir).
    if root.join("workspaces").is_dir()
        && let Ok(ws) = util::workspaces(g)
        && let Ok(list) = ws.list()
    {
        dirs.extend(list.into_iter().filter_map(|w| match w.backing {
            WorkspaceBacking::HostDir(p) => Some(p),
            _ => None,
        }));
    }
    let insensitive: Vec<String> = dirs
        .iter()
        .filter(|d| case_insensitive(d) == Some(true))
        .map(|d| d.display().to_string())
        .collect();
    if insensitive.is_empty() {
        r.add(
            "lk:workspace_case",
            Status::Ok,
            "workspace roots are case-sensitive",
        );
    } else {
        r.add(
            "lk:workspace_case",
            if selected { Status::Warn } else { Status::Info },
            format!(
                "case-insensitive volume for {}: names differing only in case (Readme/readme) \
                 collide inside libkrun guests (same as Docker Desktop on macOS); use a \
                 case-sensitive APFS volume for such workspaces",
                insensitive.join(", ")
            ),
        );
    }
}

/// Fail fast, with the fix, before handing the config to the SDK. Never falls back.
pub fn preflight(c: &LibkrunConfig) -> anyhow::Result<()> {
    const TAIL: &str = "sandcrab never falls back to another backend; `sandcrab doctor` lists \
                        every libkrun prerequisite";
    if let Err(e) = c.validate() {
        bail!("libkrun backend unavailable: {e}. {TAIL}");
    }
    if let Err(e) = hypervisor() {
        bail!("libkrun backend unavailable: {e}. {TAIL}");
    }
    if let Err(e) = helper_check(&c.helper) {
        bail!("libkrun backend unavailable: {e}. {TAIL}");
    }
    if let RootfsSource::Template { crabd } = &c.rootfs
        && !crabd.is_file()
    {
        bail!(
            "libkrun backend unavailable: static crabd {} not found (build it with \
             scripts/build-crabd.sh, or pass --crabd / SANDCRAB_CRABD). {TAIL}",
            crabd.display()
        );
    }
    Ok(())
}

/// `sandcrab doctor` checks for the libkrun backend. Missing prerequisites fail only when
/// `--backend libkrun` is selected (warn otherwise). Always reaps orphaned run dirs of dead
/// sandcrab processes (only those: live sandboxes are never touched).
/// Host-side cap of the helper: a delegated cgroup v2 leaf on Linux (`$SANDCRAB_LIBKRUN_CGROUP`),
/// none on macOS. A configured but unusable parent blocks spawns, so it is a blocker here too.
fn cgroup(c: &LibkrunConfig, r: &mut Report, blockers: &mut Vec<&str>, bad: Status) {
    #[cfg(target_os = "linux")]
    match &c.cgroup_parent {
        Some(p) => match sandcrab_sdk::libkrun::check_cgroup_parent(p) {
            Ok(()) => r.add(
                "lk:cgroup",
                Status::Ok,
                format!("helper leaves under {} (memory.max, pids.max)", p.display()),
            ),
            Err(e) => {
                blockers.push("cgroup");
                r.add("lk:cgroup", bad, e.to_string());
            }
        },
        None => r.add(
            "lk:cgroup",
            Status::Warn,
            format!(
                "no host-side memory/pids cap on the helper (VMM); set {} to a delegated cgroup \
                 v2 directory (docs/hosting.md)",
                sandcrab_sdk::libkrun::config::CGROUP_ENV
            ),
        ),
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (c, blockers, bad);
        r.add(
            "lk:cgroup",
            Status::Info,
            "no cgroups on macOS: guest RAM bounds the guest, VMM overhead is not capped",
        );
    }
}

pub fn doctor(g: &Global, r: &mut Report) {
    let selected = matches!(g.backend, Some(BackendArg::Libkrun));
    let bad = if selected { Status::Fail } else { Status::Warn };
    let c = match config(g) {
        Ok(c) => c,
        Err(e) => return r.add("libkrun", bad, e.to_string()),
    };
    let mut blockers: Vec<&str> = Vec::new();

    // Config invariants, notably the Unix socket path budget of the run dir.
    if let Err(e) = c.validate() {
        blockers.push("config");
        r.add("lk:config", bad, e.to_string());
    }

    match hypervisor() {
        Ok(m) => r.add("lk:hypervisor", Status::Ok, m),
        Err(m) => {
            blockers.push("no hypervisor");
            r.add("lk:hypervisor", bad, m);
        }
    }
    match helper_check(&c.helper) {
        Ok(m) => r.add("lk:helper", Status::Ok, m),
        Err(m) => {
            blockers.push("helper");
            r.add("lk:helper", bad, m);
        }
    }
    match Artifacts::resolve(&c) {
        Ok(a) => r.add("lk:artifacts", Status::Ok, describe(&a)),
        Err(e) => {
            blockers.push("artifacts");
            // A present-but-mismatching install is never used: always a failure.
            let status = match e {
                artifacts::ArtifactError::NotInstalled { .. } => bad,
                _ => Status::Fail,
            };
            let msg = e.to_string();
            let hint = if msg.contains("libkrun install") {
                ""
            } else {
                "; reinstall with `sandcrab libkrun install --force`"
            };
            r.add("lk:artifacts", status, format!("{msg}{hint}"));
        }
    }
    if let RootfsSource::Template { crabd } = &c.rootfs {
        if crabd.is_file() {
            r.add("lk:crabd", Status::Ok, crabd.display().to_string());
        } else {
            blockers.push("no crabd");
            r.add(
                "lk:crabd",
                bad,
                format!(
                    "{} not found (scripts/build-crabd.sh, --crabd or SANDCRAB_CRABD)",
                    crabd.display()
                ),
            );
        }
    }
    // Rootfs + scratch disks are ext4 images made with the host's mke2fs (Homebrew's
    // e2fsprogs is keg-only on macOS, so it is not on PATH by default).
    match Command::new("mke2fs").arg("-V").output() {
        Ok(o) => r.add(
            "lk:mke2fs",
            Status::Ok,
            String::from_utf8_lossy(&o.stderr)
                .lines()
                .next()
                .unwrap_or("mke2fs")
                .trim()
                .to_owned(),
        ),
        Err(e) => {
            blockers.push("no mke2fs");
            r.add(
                "lk:mke2fs",
                bad,
                format!(
                    "mke2fs not found on PATH ({e}): {}",
                    if cfg!(target_os = "macos") {
                        "`brew install e2fsprogs` and add `$(brew --prefix e2fsprogs)/sbin` to PATH"
                    } else {
                        "install e2fsprogs"
                    }
                ),
            );
        }
    }
    workspace_case(g, r, selected);
    cgroup(&c, r, &mut blockers, bad);
    netstack(&c, r);
    if c.allow_tsi_unrestricted {
        r.add(
            "lk:network",
            Status::Warn,
            "--libkrun-tsi (deprecated): `unrestricted` uses TSI; the guest can reach host \
             loopback services",
        );
    }

    match reap::reap_run_dir(&c.run_dir) {
        Ok(rep) => r.add(
            "lk:orphans",
            Status::Ok,
            format!(
                "reaped {} run dir(s), killed {} helper(s) in {}",
                rep.run_dirs.len(),
                rep.killed.len(),
                c.run_dir.display()
            ),
        ),
        Err(e) => r.add("lk:orphans", Status::Fail, e.to_string()),
    }

    if blockers.is_empty() {
        r.add("libkrun", Status::Ok, "libkrun: ready (hardware isolated)");
    } else {
        r.add(
            "libkrun",
            if selected { Status::Fail } else { Status::Info },
            format!("libkrun: unavailable ({})", blockers.join(", ")),
        );
    }
}

/// `lk:netstack`: the pinned userspace netstack for `--network unrestricted` (optional:
/// without it that policy is refused, `disabled` works).
fn netstack(c: &LibkrunConfig, r: &mut Report) {
    match netstack_pins::resolve_config(c) {
        Ok(Some(n))
            if sandcrab_sdk::libkrun::netstack::quarantined(&n.binary.path).unwrap_or(false) =>
        {
            r.add(
                "lk:netstack",
                Status::Warn,
                format!(
                    "{} {} carries com.apple.quarantine: Gatekeeper would block it behind a \
                     dialog. The backend removes it after the pin check when it starts (with \
                     verify_pins on), and so does `sandcrab libkrun install`",
                    n.kind,
                    n.binary.path.display()
                ),
            );
        }
        Ok(Some(n)) => r.add(
            "lk:netstack",
            Status::Ok,
            format!(
                "{} {} (sha256 {}): `--network unrestricted` behind the egress filter",
                n.kind,
                n.binary.path.display(),
                short(&n.binary.sha256)
            ),
        ),
        Ok(None) => r.add(
            "lk:netstack",
            Status::Info,
            format!(
                "{} not installed: `--network unrestricted` is refused (run `sandcrab libkrun \
                 install`)",
                netstack_pins::host_pin().map_or("netstack", |p| p.kind.as_str())
            ),
        ),
        Err(e) => r.add("lk:netstack", Status::Fail, e.to_string()),
    }
}

#[derive(Debug, Subcommand)]
pub enum LibkrunCmd {
    /// Install the pinned libkrun + libkrunfw into <state root>/artifacts/libkrun/<version>/
    /// and the signed `sandcrab-krun` helper into <state root>/bin/.
    ///
    /// Default on macOS with Homebrew: copy the pinned bottles (`brew install libkrun
    /// libkrunfw` first) and verify their SHA-256s. `--from-source` builds both from the pinned
    /// git tags (scripts/fetch-libkrun.sh; the only option without Homebrew). Linux always
    /// builds libkrun from source.
    Install {
        /// Build libkrun/libkrunfw from the pinned git tags instead of using Homebrew.
        #[arg(long)]
        from_source: bool,

        /// Linux: also build libkrunfw (the guest kernel) from source instead of the pinned
        /// release asset.
        #[arg(long)]
        fw_from_source: bool,

        /// Reinstall the libraries even if a verified install is present.
        #[arg(long)]
        force: bool,

        /// Helper binary to install (default: `sandcrab-krun` next to this executable).
        #[arg(long, conflicts_with = "no_helper")]
        helper: Option<PathBuf>,

        /// Install the libraries only.
        #[arg(long)]
        no_helper: bool,

        /// Skip the pinned userspace netstack (gvproxy on macOS, passt on Linux) that
        /// `--network unrestricted` needs.
        #[arg(long)]
        no_netstack: bool,

        /// Print the steps without changing anything.
        #[arg(long)]
        dry_run: bool,
    },
}

pub fn libkrun_cmd(g: &Global, cmd: LibkrunCmd) -> anyhow::Result<u8> {
    match cmd {
        LibkrunCmd::Install {
            from_source,
            fw_from_source,
            force,
            helper,
            no_helper,
            no_netstack,
            dry_run,
        } => install(
            g,
            &InstallOpts {
                from_source,
                fw_from_source,
                force,
                helper,
                no_helper,
                no_netstack,
                dry_run,
            },
        ),
    }
}

struct InstallOpts {
    from_source: bool,
    fw_from_source: bool,
    force: bool,
    helper: Option<PathBuf>,
    no_helper: bool,
    no_netstack: bool,
    dry_run: bool,
}

fn brew_available() -> bool {
    Command::new("brew")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// `scripts/fetch-libkrun.sh` of a source checkout (cwd first, then the one this binary was
/// built from).
fn fetch_script() -> anyhow::Result<PathBuf> {
    let candidates = [
        PathBuf::from("scripts/fetch-libkrun.sh"),
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/fetch-libkrun.sh"),
    ];
    candidates.into_iter().find(|p| p.is_file()).ok_or_else(|| {
        anyhow::anyhow!(
            "scripts/fetch-libkrun.sh not found: run from a sandcrab source checkout for \
                 from-source builds"
        )
    })
}

fn install(g: &Global, o: &InstallOpts) -> anyhow::Result<u8> {
    let home = home(g)?;
    let dir = artifacts::default_dir(&home);
    let pins = artifacts::host_pins()?;
    // Review R18: never write pins and records into a tree others can write. A loose tree
    // owned by this user is tightened to 0700 (logged); a symlinked root or another owner is
    // refused, like the backend does at construction.
    if !o.dry_run {
        use sandcrab_sdk::core::private_dir;
        let bin = home.join("bin");
        let net = netstack_pins::host_pin().map(|p| netstack_pins::default_dir(&home, p));
        let mut dirs = vec![bin.as_path(), dir.as_path()];
        dirs.extend(net.as_deref().ok());
        private_dir::secure_state_tree("libkrun", &home, &dirs, private_dir::current_uid())?;
    }

    // 1. Libraries.
    let installed = if o.force || o.from_source || o.fw_from_source {
        None
    } else {
        Artifacts::resolve_dir(&dir, true).ok()
    };
    let libs: serde_json::Value = if let Some(a) = installed {
        eprintln!("libkrun: already installed: {}", describe(&a));
        json!({ "mode": "present", "dir": a.dir, "libkrun_sha256": a.libkrun.sha256,
                "libkrunfw_sha256": a.libkrunfw.sha256 })
    } else if !o.from_source && !o.fw_from_source && pins.libkrun.homebrew.is_some() {
        if !brew_available() {
            bail!(
                "Homebrew not found: building from source is the only option on this host; \
                 pass --from-source (needs Rust, clang, lld; builds the pinned git tags)"
            );
        }
        if o.dry_run {
            eprintln!(
                "libkrun: would copy the pinned Homebrew bottles (libkrun {}, libkrunfw {}) into {}",
                artifacts::LIBKRUN_VERSION,
                artifacts::LIBKRUNFW_VERSION,
                dir.display()
            );
            json!({ "mode": "homebrew", "dry_run": true, "dir": dir })
        } else {
            let a = artifacts::install_homebrew(&home).context(
                "installing the Homebrew bottles (`brew install libkrun libkrunfw` first, or \
                 pass --from-source)",
            )?;
            eprintln!("libkrun: installed {}", describe(&a));
            json!({ "mode": "homebrew", "dir": a.dir, "libkrun_sha256": a.libkrun.sha256,
                    "libkrunfw_sha256": a.libkrunfw.sha256 })
        }
    } else {
        let script = fetch_script()?;
        let mut cmd = Command::new("bash");
        cmd.arg(&script)
            .arg("--home")
            .arg(&home)
            .arg("--no-netstack");
        if o.from_source {
            cmd.arg("--from-source");
        }
        if o.fw_from_source {
            cmd.arg("--fw-from-source");
        }
        if o.dry_run {
            cmd.arg("--dry-run");
        }
        eprintln!("libkrun: running {}", script.display());
        let status = cmd
            .status()
            .with_context(|| format!("running {}", script.display()))?;
        if !status.success() {
            bail!("{} failed ({status})", script.display());
        }
        if o.dry_run {
            json!({ "mode": "source", "dry_run": true, "dir": dir })
        } else {
            let a = Artifacts::resolve_dir(&dir, true)
                .context("the from-source install does not verify against the pins")?;
            eprintln!("libkrun: installed {}", describe(&a));
            json!({ "mode": "source", "dir": a.dir, "libkrun_sha256": a.libkrun.sha256,
                    "libkrunfw_sha256": a.libkrunfw.sha256 })
        }
    };

    // 2. Netstack (network `unrestricted`).
    let netstack = if o.no_netstack {
        serde_json::Value::Null
    } else {
        install_netstack(&home, o)?
    };

    // 3. Helper.
    let helper = if o.no_helper {
        serde_json::Value::Null
    } else {
        let src = helper_source(o.helper.as_deref())?;
        let dst = home.join("bin").join(HELPER_BIN_NAME);
        if o.dry_run {
            eprintln!(
                "libkrun: would install {} → {} and sign it",
                src.display(),
                dst.display()
            );
            json!({ "source": src, "path": dst, "dry_run": true })
        } else {
            let signed = install_helper(&src, &dst)?;
            eprintln!("libkrun: helper {} ({signed})", dst.display());
            json!({ "source": src, "path": dst, "signature": signed })
        }
    };

    if g.json {
        print_json(&json!({ "libraries": libs, "netstack": netstack, "helper": helper }))?;
    }
    Ok(0)
}

/// The pinned netstack: the gvproxy release asset (macOS, downloaded and SHA-256 checked) or
/// passt built from its pinned tag (Linux, `scripts/fetch-libkrun.sh --netstack-only`).
fn install_netstack(home: &Path, o: &InstallOpts) -> anyhow::Result<serde_json::Value> {
    let pin = netstack_pins::host_pin()?;
    let dir = netstack_pins::default_dir(home, pin);
    if !o.force
        && let Ok(Some(n)) = netstack_pins::resolve_dir(&dir, pin)
    {
        eprintln!(
            "libkrun: netstack already installed: {} {}",
            n.kind,
            n.path.display()
        );
        if !o.dry_run && netstack_pins::clear_quarantine(&n)? {
            eprintln!("libkrun: removed com.apple.quarantine from it after the digest check");
        }
        return Ok(
            json!({ "mode": "present", "kind": n.kind, "path": n.path, "sha256": n.sha256 }),
        );
    }
    if pin.release.is_some() {
        if o.dry_run {
            eprintln!(
                "libkrun: would download {} {} into {}",
                pin.kind,
                pin.version,
                dir.display()
            );
            return Ok(json!({ "mode": "release", "dry_run": true, "dir": dir }));
        }
        let n = netstack_pins::install_release(home)
            .with_context(|| format!("installing {} {}", pin.kind, pin.version))?;
        eprintln!(
            "libkrun: netstack {} {} ({})",
            n.kind,
            n.path.display(),
            short(&n.sha256)
        );
        return Ok(
            json!({ "mode": "release", "kind": n.kind, "path": n.path, "sha256": n.sha256 }),
        );
    }
    let script = fetch_script()?;
    let mut cmd = Command::new("bash");
    cmd.arg(&script)
        .arg("--home")
        .arg(home)
        .arg("--netstack-only");
    if o.dry_run {
        cmd.arg("--dry-run");
    }
    eprintln!("libkrun: building {} via {}", pin.kind, script.display());
    let status = cmd
        .status()
        .with_context(|| format!("running {}", script.display()))?;
    if !status.success() {
        bail!("{} --netstack-only failed ({status})", script.display());
    }
    if o.dry_run {
        return Ok(json!({ "mode": "source", "dry_run": true, "dir": dir }));
    }
    let n = netstack_pins::resolve_dir(&dir, pin)?.context("the netstack build did not install")?;
    eprintln!(
        "libkrun: netstack {} {} ({})",
        n.kind,
        n.path.display(),
        short(&n.sha256)
    );
    Ok(json!({ "mode": "source", "kind": n.kind, "path": n.path, "sha256": n.sha256 }))
}

/// `--helper`, else `sandcrab-krun` next to this executable.
fn helper_source(arg: Option<&Path>) -> anyhow::Result<PathBuf> {
    if let Some(p) = arg {
        if !p.is_file() {
            bail!("helper {} not found", p.display());
        }
        return Ok(p.to_path_buf());
    }
    let exe = std::env::current_exe().context("locating this executable")?;
    let sibling = exe.with_file_name(HELPER_BIN_NAME);
    if sibling.is_file() {
        return Ok(sibling);
    }
    bail!(
        "`{HELPER_BIN_NAME}` not found next to {}: build it with `cargo build --release -p \
         sandcrab-backend-libkrun --bin {HELPER_BIN_NAME}` and pass --helper <path> (or \
         --no-helper to install the libraries only)",
        exe.display()
    )
}

/// Copy `src` to a temp file beside `dst`, sign it (macOS), rename it into place, then write
/// its install record (sha256 + stat stamp, `<helper>.pin.json`) that the backend checks before
/// every launch. The record is taken after the rename: renaming a file changes its ctime.
fn install_helper(src: &Path, dst: &Path) -> anyhow::Result<String> {
    let bin = dst.parent().context("helper path has no parent")?;
    // 0700 whatever the umask (review R18): whoever can write here can replace the helper.
    artifacts::create_private_dir_all(bin)
        .with_context(|| format!("creating {}", bin.display()))?;
    let tmp = bin.join(format!(".{HELPER_BIN_NAME}.{}.tmp", std::process::id()));
    let result = (|| {
        fs::copy(src, &tmp).with_context(|| format!("copying {}", src.display()))?;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755))?;
        let signed = sign(&tmp)?;
        fs::rename(&tmp, dst).with_context(|| format!("installing {}", dst.display()))?;
        let record =
            HelperRecord::record(dst).with_context(|| format!("recording {}", dst.display()))?;
        Ok(format!("{signed}; sha256 {}", short(&record.sha256)))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Sign with the hypervisor entitlement (`scripts/sign-krun-helper.sh` logic):
/// `$SANDCRAB_SIGN_IDENTITY` (default `-`, ad hoc); a Developer ID also gets the hardened
/// runtime and a secure timestamp. Verifies the signature and the entitlement afterwards.
#[cfg(target_os = "macos")]
fn sign(bin: &Path) -> anyhow::Result<String> {
    let identity = std::env::var(SIGN_IDENTITY_ENV)
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "-".into());
    // Hardened runtime for a Developer ID (or on request): it then needs
    // disable-library-validation to load libkrun at all (review R10, config.rs).
    let runtime = identity != "-" || std::env::var(SIGN_RUNTIME_ENV).is_ok_and(|v| v == "1");
    let plist = bin.with_extension("entitlements.plist");
    fs::write(
        &plist,
        if runtime {
            ENTITLEMENTS_HARDENED_PLIST
        } else {
            ENTITLEMENTS_PLIST
        },
    )?;
    let mut cmd = Command::new("/usr/bin/codesign");
    cmd.args(["--force", "--sign", &identity, "--entitlements"])
        .arg(&plist);
    if runtime {
        cmd.args(["--options", "runtime"]);
    }
    if identity != "-" {
        cmd.arg("--timestamp");
    }
    let out = cmd.arg(bin).output();
    let _ = fs::remove_file(&plist);
    let out = out.context("running codesign")?;
    if !out.status.success() {
        bail!(
            "codesign failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let verify = Command::new("/usr/bin/codesign")
        .args(["--verify", "--strict"])
        .arg(bin)
        .output()
        .context("running codesign --verify")?;
    if !verify.status.success() {
        bail!(
            "codesign --verify failed: {}",
            String::from_utf8_lossy(&verify.stderr).trim()
        );
    }
    if !has_entitlement(bin) {
        bail!(
            "{} is signed but lacks {HYPERVISOR_ENTITLEMENT}",
            bin.display()
        );
    }
    let who = if identity == "-" {
        "ad-hoc signed".to_owned()
    } else {
        format!("signed by {identity}")
    };
    Ok(if runtime {
        format!(
            "{who} (hardened runtime) with {HYPERVISOR_ENTITLEMENT} + disable-library-validation"
        )
    } else {
        format!("{who} with {HYPERVISOR_ENTITLEMENT}")
    })
}

/// Linux KVM needs no signature.
#[cfg(not(target_os = "macos"))]
fn sign(_bin: &Path) -> anyhow::Result<String> {
    Ok("unsigned (not needed on Linux)".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn case_probe() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path().join("CaseProbe");
        fs::create_dir(&d).unwrap();
        let alt_exists = tmp.path().join("caseprobe").exists();
        assert_eq!(case_insensitive(&d), Some(alt_exists));
        // A not-yet-existing child answers for its nearest existing ancestor.
        assert_eq!(case_insensitive(&d.join("missing/x")), Some(alt_exists));
    }

    #[test]
    fn helper_must_be_executable() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join(HELPER_BIN_NAME);
        assert!(helper_check(&p).unwrap_err().contains("libkrun install"));
        fs::write(&p, b"#!/bin/sh\n").unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(helper_check(&p).is_err());
    }

    #[test]
    fn missing_helper_source_is_an_error() {
        let e = helper_source(Some(Path::new("/nonexistent/sandcrab-krun"))).unwrap_err();
        assert!(e.to_string().contains("not found"), "{e}");
    }
}
