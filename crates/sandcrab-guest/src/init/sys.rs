//! Linux system operations of `crabd init` (PID 1). All syscalls go through `rustix`.
//!
//! These need root inside a booted microVM and are exercised by the Firecracker e2e suite,
//! not by unit tests (pure planning lives in `config`, `drives`, `command`).

use std::{
    ffi::CString,
    fs, io,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use rustix::{
    mount::{MountFlags, MountPropagationFlags, UnmountFlags},
    process::{Pid, Signal},
    system::RebootCommand,
};
use tracing::{debug, info, warn};

fn ctx(e: impl Into<io::Error>, what: impl std::fmt::Display) -> io::Error {
    let e: io::Error = e.into();
    io::Error::new(e.kind(), format!("{what}: {e}"))
}

/// `mount(2)` with context in the error.
pub fn mount(
    source: &str,
    target: &Path,
    fstype: &str,
    flags: MountFlags,
    data: &str,
) -> io::Result<()> {
    let data = CString::new(data).map_err(|_| io::Error::other("NUL in mount data"))?;
    let data = (!data.as_bytes().is_empty()).then_some(data.as_c_str());
    rustix::mount::mount(source, target, fstype, flags, data).map_err(|e| {
        ctx(
            e,
            format!("mount {fstype} {source} on {}", target.display()),
        )
    })
}

fn mkdir_p(p: &Path) -> io::Result<()> {
    fs::create_dir_all(p).map_err(|e| ctx(e, format!("mkdir {}", p.display())))
}

const NOSUID_NODEV_NOEXEC: MountFlags = MountFlags::NOSUID
    .union(MountFlags::NODEV)
    .union(MountFlags::NOEXEC);

/// Step 1: `/proc`, `/sys`, `/dev` (skipped when already mounted, e.g. devtmpfs automount).
pub fn early_mounts() -> io::Result<()> {
    if !Path::new("/proc/self").exists() {
        mount("proc", Path::new("/proc"), "proc", NOSUID_NODEV_NOEXEC, "")?;
    }
    if !Path::new("/sys/kernel").exists() {
        mount("sysfs", Path::new("/sys"), "sysfs", NOSUID_NODEV_NOEXEC, "")?;
    }
    if !Path::new("/dev/null").exists() {
        mount(
            "devtmpfs",
            Path::new("/dev"),
            "devtmpfs",
            MountFlags::NOSUID,
            "mode=0755",
        )?;
    }
    Ok(())
}

pub const SCRATCH_MNT: &str = "/mnt/scratch";
pub const NEWROOT_MNT: &str = "/mnt/newroot";

/// Step 2: writable root = overlay(lower = read-only `/`, upper = scratch drive), then
/// `pivot_root` into it. `/proc /sys /dev` and the scratch mount move along.
pub fn overlay_root(scratch_dev: &Path) -> io::Result<()> {
    // Mount events must not propagate back into the old root while we move things around.
    rustix::mount::mount_change(
        "/",
        MountPropagationFlags::PRIVATE | MountPropagationFlags::REC,
    )
    .map_err(|e| ctx(e, "make / private"))?;
    let scratch = Path::new(SCRATCH_MNT);
    let newroot = Path::new(NEWROOT_MNT);
    mount(
        &scratch_dev.to_string_lossy(),
        scratch,
        "ext4",
        MountFlags::NOATIME | MountFlags::NOSUID | MountFlags::NODEV,
        "",
    )?;
    mkdir_p(&scratch.join("upper"))?;
    mkdir_p(&scratch.join("work"))?;
    mount(
        "overlay",
        newroot,
        "overlay",
        MountFlags::NOATIME,
        "lowerdir=/,upperdir=/mnt/scratch/upper,workdir=/mnt/scratch/work",
    )?;
    for dir in ["/proc", "/sys", "/dev", SCRATCH_MNT] {
        let to = newroot.join(dir.trim_start_matches('/'));
        rustix::mount::mount_move(dir, &to)
            .map_err(|e| ctx(e, format!("move {dir} to {}", to.display())))?;
    }
    // pivot_root(".", ".") stacks the old root on top of the new one; detaching it leaves
    // the overlay as `/` without needing a put_old directory.
    rustix::process::chdir(newroot).map_err(|e| ctx(e, "chdir new root"))?;
    rustix::process::pivot_root(".", ".").map_err(|e| ctx(e, "pivot_root"))?;
    rustix::mount::unmount(".", UnmountFlags::DETACH).map_err(|e| ctx(e, "detach old root"))?;
    rustix::process::chdir("/").map_err(|e| ctx(e, "chdir /"))?;
    info!("root is now overlay(rootfs, scratch)");
    Ok(())
}

/// Step 3: `/dev/pts`, `/dev/shm`, `/run`; `/tmp` stays on the overlay (disk, not RAM).
pub fn late_mounts() -> io::Result<()> {
    mkdir_p(Path::new("/dev/pts"))?;
    mount(
        "devpts",
        Path::new("/dev/pts"),
        "devpts",
        MountFlags::NOSUID | MountFlags::NOEXEC,
        "ptmxmode=0666,mode=0620,gid=5",
    )?;
    mkdir_p(Path::new("/dev/shm"))?;
    mount(
        "tmpfs",
        Path::new("/dev/shm"),
        "tmpfs",
        MountFlags::NOSUID | MountFlags::NODEV,
        "mode=1777",
    )?;
    mkdir_p(Path::new("/run"))?;
    mount(
        "tmpfs",
        Path::new("/run"),
        "tmpfs",
        MountFlags::NOSUID | MountFlags::NODEV,
        "mode=0755,size=5%",
    )?;
    mkdir_p(Path::new("/tmp"))?;
    fs::set_permissions("/tmp", fs::Permissions::from_mode(0o1777))
        .map_err(|e| ctx(e, "chmod /tmp"))?;
    Ok(())
}

pub fn set_hostname(name: &str) -> io::Result<()> {
    rustix::system::sethostname(name.as_bytes()).map_err(|e| ctx(e, "sethostname"))
}

/// Set `CLOCK_REALTIME` from the host's wall clock.
pub fn set_clock(unix_ms: u64) -> io::Result<()> {
    let ts = rustix::time::Timespec {
        tv_sec: i64::try_from(unix_ms / 1000).unwrap_or(i64::MAX),
        tv_nsec: i64::try_from((unix_ms % 1000) * 1_000_000).unwrap_or(0),
    };
    rustix::time::clock_settime(rustix::time::ClockId::Realtime, ts)
        .map_err(|e| ctx(e, "clock_settime"))
}

/// Ctrl-Alt-Del → `SIGINT` to PID 1 instead of an immediate reboot.
pub fn disable_cad() -> io::Result<()> {
    rustix::system::reboot(RebootCommand::CadOff).map_err(|e| ctx(e, "reboot(CAD_OFF)"))
}

/// Mount a workspace drive (ext4). Read-only adds `MS_RDONLY` and `data` (`noload`).
pub fn mount_workspace(dev: &Path, target: &Path, read_only: bool, data: &str) -> io::Result<()> {
    mkdir_p(target)?;
    let mut flags = MountFlags::NOATIME | MountFlags::NOSUID | MountFlags::NODEV;
    if read_only {
        flags |= MountFlags::RDONLY;
    }
    mount(&dev.to_string_lossy(), target, "ext4", flags, data)
}

/// Guest uptime in ms from `/proc/uptime`.
pub fn uptime_ms() -> u64 {
    fs::read_to_string("/proc/uptime")
        .ok()
        .and_then(|s| s.split_whitespace().next()?.parse::<f64>().ok())
        .map_or(0, |secs| (secs * 1000.0) as u64)
}

/// User-space processes other than PID 1 (kernel threads have an empty cmdline).
pub fn user_pids() -> Vec<Pid> {
    let Ok(rd) = fs::read_dir("/proc") else {
        return Vec::new();
    };
    rd.filter_map(Result::ok)
        .filter_map(|e| e.file_name().to_str()?.parse::<i32>().ok())
        .filter(|&pid| pid > 1)
        .filter(|pid| fs::read(format!("/proc/{pid}/cmdline")).is_ok_and(|c| !c.is_empty()))
        .filter_map(Pid::from_raw)
        .collect()
}

pub fn signal_all(sig: Signal) {
    for pid in user_pids() {
        if let Err(e) = rustix::process::kill_process(pid, sig) {
            debug!(pid = pid.as_raw_nonzero().get(), error = %e, "kill failed");
        }
    }
}

/// SIGTERM everything, wait up to `grace` (polling), then SIGKILL what is left.
pub async fn terminate_all(grace: Duration) {
    signal_all(Signal::TERM);
    let deadline = Instant::now() + grace;
    while Instant::now() < deadline && !user_pids().is_empty() {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let left = user_pids().len();
    if left > 0 {
        warn!(left, "processes ignored SIGTERM; sending SIGKILL");
        signal_all(Signal::KILL);
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// `sync`, unmount `targets` (deepest first), sync again. Failures fall back to a lazy
/// detach; the final `sync` is what makes data durable either way.
pub fn sync_and_unmount(targets: &[PathBuf]) {
    rustix::fs::sync();
    let mut sorted: Vec<_> = targets.to_vec();
    sorted.sort_by_key(|p| std::cmp::Reverse(p.components().count()));
    for t in &sorted {
        if let Err(e) = rustix::mount::unmount(t, UnmountFlags::empty()) {
            warn!(target = %t.display(), error = %e, "unmount failed; detaching");
            let _ = rustix::mount::unmount(t, UnmountFlags::DETACH);
        }
    }
    rustix::fs::sync();
}

/// Reboot the guest. With `reboot=k` on the kernel cmdline Firecracker treats this as VM
/// exit (the design picks `RB_AUTOBOOT` over `RB_POWER_OFF` for that reason). Only returns
/// on failure.
pub fn reboot_now() -> io::Error {
    rustix::fs::sync();
    match rustix::system::reboot(RebootCommand::Restart) {
        Ok(()) => io::Error::other("reboot returned"),
        Err(e) => ctx(e, "reboot(RB_AUTOBOOT)"),
    }
}

/// Make `dir` (created if missing) owned by `uid:gid` — used only for a `default_cwd` that
/// `crabd init` itself had to create.
pub fn create_owned_dir(dir: &Path, uid: u32, gid: u32) -> io::Result<()> {
    if dir.exists() {
        return Ok(());
    }
    mkdir_p(dir)?;
    std::os::unix::fs::chown(dir, Some(uid), Some(gid))
        .map_err(|e| ctx(e, format!("chown {}", dir.display())))
}
