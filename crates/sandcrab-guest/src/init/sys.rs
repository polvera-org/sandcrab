//! Linux system operations of `crabd init` (PID 1). All syscalls go through `rustix`.
//!
//! These need root inside a booted microVM and are exercised by the Firecracker e2e suite,
//! not by unit tests (pure planning lives in `config`, `drives`, `command`).

use std::{
    ffi::CString,
    fs, io,
    os::fd::{AsFd, AsRawFd},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use rustix::{
    io::Errno,
    mount::{MountFlags, MountPropagationFlags, UnmountFlags},
    process::{Pid, Signal},
    system::RebootCommand,
};
use tracing::{debug, info, warn};

use super::dirs;

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
    raw_mount(source, target, fstype, flags, data)?.map_err(|e| {
        ctx(
            e,
            format!("mount {fstype} {source} on {}", target.display()),
        )
    })
}

/// `mount(2)`; the outer error is bad input, the inner one the syscall's errno.
fn raw_mount(
    source: &str,
    target: &Path,
    fstype: &str,
    flags: MountFlags,
    data: &str,
) -> io::Result<Result<(), Errno>> {
    let data = CString::new(data).map_err(|_| io::Error::other("NUL in mount data"))?;
    let data = (!data.as_bytes().is_empty()).then_some(data.as_c_str());
    Ok(rustix::mount::mount(source, target, fstype, flags, data))
}

/// Outcome of [`mount_unless_busy`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mounted {
    Now,
    /// `EBUSY`: something (libkrun's `init.krun`) already mounted it.
    Already,
}

/// [`mount`], but `EBUSY` ("already mounted") is success. Used for the pseudo filesystems
/// that libkrun's `init.krun` mounts before it execs `crabd init` as PID 1.
pub fn mount_unless_busy(
    source: &str,
    target: &Path,
    fstype: &str,
    flags: MountFlags,
    data: &str,
) -> io::Result<Mounted> {
    match raw_mount(source, target, fstype, flags, data)? {
        Ok(()) => Ok(Mounted::Now),
        Err(Errno::BUSY) => {
            debug!(target = %target.display(), fstype, "already mounted");
            Ok(Mounted::Already)
        }
        Err(e) => Err(ctx(
            e,
            format!("mount {fstype} {source} on {}", target.display()),
        )),
    }
}

/// Whether `p` is a mount point: its device differs from its parent's.
fn is_mount_point(p: &Path) -> bool {
    let Some(parent) = p.parent() else {
        return true;
    };
    match (rustix::fs::lstat(p), rustix::fs::stat(parent)) {
        (Ok(a), Ok(b)) => a.st_dev != b.st_dev,
        _ => false,
    }
}

fn mkdir_p(p: &Path) -> io::Result<()> {
    fs::create_dir_all(p).map_err(|e| ctx(e, format!("mkdir {}", p.display())))
}

const NOSUID_NODEV_NOEXEC: MountFlags = MountFlags::NOSUID
    .union(MountFlags::NODEV)
    .union(MountFlags::NOEXEC);

/// Step 1: `/proc`, `/sys`, `/dev`. Skipped when already mounted: by devtmpfs automount
/// (Firecracker), or by libkrun's `init.krun`, which mounts `/proc /sys /dev /dev/pts
/// /dev/shm` before it execs us; `EBUSY` from a race with either counts as mounted.
pub fn early_mounts() -> io::Result<()> {
    if !Path::new("/proc/self").exists() {
        mount_unless_busy("proc", Path::new("/proc"), "proc", NOSUID_NODEV_NOEXEC, "")?;
    }
    if !Path::new("/sys/kernel").exists() {
        mount_unless_busy("sysfs", Path::new("/sys"), "sysfs", NOSUID_NODEV_NOEXEC, "")?;
    }
    if !Path::new("/dev/null").exists() {
        mount_unless_busy(
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

/// Step 3: `/dev/pts`, `/dev/shm`, `/run`, then `/tmp` (see [`mount_tmp`]).
/// A target that is already a mount point is left alone: under libkrun `init.krun`
/// mounted `/dev/pts` and `/dev/shm`, and they moved along with `/dev`.
pub fn late_mounts() -> io::Result<()> {
    for (source, target, fstype, flags, data) in [
        (
            "devpts",
            "/dev/pts",
            "devpts",
            MountFlags::NOSUID | MountFlags::NOEXEC,
            "ptmxmode=0666,mode=0620,gid=5",
        ),
        (
            "tmpfs",
            "/dev/shm",
            "tmpfs",
            MountFlags::NOSUID | MountFlags::NODEV,
            "mode=1777",
        ),
        (
            "tmpfs",
            "/run",
            "tmpfs",
            MountFlags::NOSUID | MountFlags::NODEV,
            "mode=0755,size=5%",
        ),
    ] {
        let target = Path::new(target);
        mkdir_p(target)?;
        if is_mount_point(target) {
            debug!(target = %target.display(), "already mounted");
            continue;
        }
        mount_unless_busy(source, target, fstype, flags, data)?;
    }
    mount_tmp()
}

/// `/tmp` mount flags: the Docker backend's `nosuid,nodev,noexec` tmpfs.
pub const TMP_FLAGS: MountFlags = NOSUID_NODEV_NOEXEC;

/// `/tmp` tmpfs data: `mode=1777` and the Docker backend's size cap.
#[must_use]
pub fn tmp_mount_data(size_mb: u64) -> String {
    format!("mode=1777,size={size_mb}m")
}

/// A fresh size-limited tmpfs on `/tmp`, `nosuid,nodev,noexec`, like the Docker backend's
/// (`HostConfig.tmpfs`): nothing under `/tmp` can be executed or `mmap`ed `PROT_EXEC`, and
/// filling it cannot exhaust the scratch drive. It counts against guest RAM, as Docker's
/// counts against the container's memory limit. Always mounted (stacked over whatever the
/// image had in `/tmp`, which Docker hides the same way).
fn mount_tmp() -> io::Result<()> {
    let tmp = Path::new("/tmp");
    mkdir_p(tmp)?;
    mount(
        "tmpfs",
        tmp,
        "tmpfs",
        TMP_FLAGS,
        &tmp_mount_data(sandcrab_protocol::init::SANDBOX_TMP_SIZE_MB),
    )
}

/// cgroup v2 mount point.
pub const CGROUP_ROOT: &str = "/sys/fs/cgroup";
/// The cgroup holding `crabd serve` and every tool process.
pub const SANDBOX_CGROUP: &str = "/sys/fs/cgroup/sandbox";

/// Whether a `cgroup.controllers` text lists `name`.
#[must_use]
pub fn has_controller(controllers: &str, name: &str) -> bool {
    controllers.split_whitespace().any(|c| c == name)
}

/// The fork-bomb cap: mount cgroup v2 (unless `init.krun` already did), enable the `pids`
/// controller and create [`SANDBOX_CGROUP`] with `pids.max = pids_max`. `crabd serve` is
/// spawned into it ([`enter_cgroup`]), so the cap covers every sandbox task, whatever its
/// uid (root included), as Docker's `pids_limit` does. Fails when the kernel has no `pids`
/// controller; the caller falls back to `RLIMIT_NPROC`.
pub fn setup_pids_cgroup(pids_max: u32) -> io::Result<PathBuf> {
    let root = Path::new(CGROUP_ROOT);
    if !root.join("cgroup.controllers").exists() {
        mount_unless_busy("cgroup2", root, "cgroup2", NOSUID_NODEV_NOEXEC, "")?;
    }
    let controllers = fs::read_to_string(root.join("cgroup.controllers"))
        .map_err(|e| ctx(e, "read cgroup.controllers"))?;
    if !has_controller(&controllers, "pids") {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!("no pids cgroup controller (have: {})", controllers.trim()),
        ));
    }
    // The root cgroup may hold processes (PID 1) and still delegate controllers.
    fs::write(root.join("cgroup.subtree_control"), "+pids")
        .map_err(|e| ctx(e, "enable the pids controller"))?;
    let dir = Path::new(SANDBOX_CGROUP);
    match fs::create_dir(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(ctx(e, format!("mkdir {}", dir.display()))),
    }
    fs::write(dir.join("pids.max"), pids_max.to_string()).map_err(|e| ctx(e, "write pids.max"))?;
    Ok(dir.to_path_buf())
}

/// Move this whole process (every thread) into cgroup `dir`.
pub fn enter_cgroup(dir: &Path) -> io::Result<()> {
    fs::write(dir.join("cgroup.procs"), std::process::id().to_string())
        .map_err(|e| ctx(e, format!("enter cgroup {}", dir.display())))
}

/// Make the cgroup v2 mount read-only once `crabd serve` is placed. Docker mounts the
/// container's cgroupfs read-only too. The files are root-owned, so this matters when the
/// sandbox user is uid 0: without capabilities it could still write `pids.max` through the
/// owner permission bits, but it can neither write a read-only mount nor remount it.
pub fn seal_cgroupfs() -> io::Result<()> {
    rustix::mount::mount_remount(
        CGROUP_ROOT,
        MountFlags::BIND | MountFlags::RDONLY | NOSUID_NODEV_NOEXEC,
        "",
    )
    .map_err(|e| ctx(e, "remount cgroupfs read-only"))
}

/// The `RLIMIT_NPROC` fallback when there is no `pids` controller: set on PID 1 (root is
/// exempt from it) and inherited by `crabd serve`. It counts the tasks of the sandbox uid,
/// which in the guest are exactly the sandbox's, but the kernel never applies it to uid 0.
pub fn limit_nproc(max: u32) -> io::Result<()> {
    let lim = rustix::process::Rlimit {
        current: Some(u64::from(max)),
        maximum: Some(u64::from(max)),
    };
    rustix::process::setrlimit(rustix::process::Resource::Nproc, lim)
        .map_err(|e| ctx(e, "setrlimit(RLIMIT_NPROC)"))
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

/// Set `RLIMIT_CORE` to 0 (soft and hard) for PID 1 and everything it spawns, so a
/// crashing process never writes `core.*` files into a (persistent) workspace cwd.
/// Matches the Docker backend's `core=0` ulimit.
pub fn disable_core_dumps() -> io::Result<()> {
    let zero = rustix::process::Rlimit {
        current: Some(0),
        maximum: Some(0),
    };
    rustix::process::setrlimit(rustix::process::Resource::Core, zero)
        .map_err(|e| ctx(e, "setrlimit(RLIMIT_CORE)"))
}

/// Mount one planned workspace on `dir`, an fd of its (already created, symlink-checked)
/// mount point; the mount goes through `/proc/self/fd/N` so the path is not re-resolved.
/// `source` is the block device path, or the virtio-fs tag.
///
/// Flags: `MS_NOSUID | MS_NODEV`, plus `MS_NOATIME` on block drives and `MS_RDONLY` for
/// read-only mounts (for virtio-fs the host enforces read-only as well).
pub fn mount_workspace(
    source: &str,
    dir: impl AsFd,
    step: &crate::init::config::MountStep,
) -> io::Result<()> {
    let mut flags = MountFlags::NOSUID | MountFlags::NODEV;
    if step.noatime() {
        flags |= MountFlags::NOATIME;
    }
    if step.read_only {
        flags |= MountFlags::RDONLY;
    }
    let via = PathBuf::from(format!("/proc/self/fd/{}", dir.as_fd().as_raw_fd()));
    let fstype = step.fstype();
    raw_mount(source, &via, fstype, flags, step.data())?.map_err(|e| {
        ctx(
            e,
            format!("mount {fstype} {source} on {}", step.target.display()),
        )
    })
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
/// `crabd init` itself had to create. An existing directory is left alone (`crabd serve`
/// enters it unprivileged, so a symlink there grants nothing); creating one never follows
/// a symlink, so root cannot be steered into creating and chowning a path elsewhere.
pub fn create_owned_dir(dir: &Path, uid: u32, gid: u32) -> io::Result<()> {
    if dir.is_dir() {
        return Ok(());
    }
    let d = dirs::ensure_dir_in(Path::new("/"), dir)?;
    if d.created {
        dirs::chown_fd(&d.fd, uid, gid).map_err(|e| ctx(e, format!("chown {}", dir.display())))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Same tmpfs as the Docker backend's `HostConfig.tmpfs["/tmp"]`
    /// (`rw,nosuid,nodev,noexec,size=256m,mode=1777`).
    #[test]
    fn tmp_matches_the_docker_tmpfs() {
        assert!(TMP_FLAGS.contains(MountFlags::NOSUID | MountFlags::NODEV | MountFlags::NOEXEC));
        assert!(!TMP_FLAGS.contains(MountFlags::RDONLY));
        assert_eq!(
            tmp_mount_data(sandcrab_protocol::init::SANDBOX_TMP_SIZE_MB),
            "mode=1777,size=256m"
        );
    }

    #[test]
    fn controller_lookup_is_exact() {
        assert!(has_controller("cpuset cpu io memory pids\n", "pids"));
        assert!(!has_controller("cpuset cpu io memory\n", "pids"));
        assert!(!has_controller("pidsx", "pids"));
        assert!(!has_controller("", "pids"));
    }
}
