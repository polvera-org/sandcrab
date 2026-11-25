//! `sandcrab-krun`: the per-sandbox libkrun VMM helper (design §2).
//!
//! 1. Read one [`HelperConfig`] JSON line from stdin and validate it (exit 70 on failure).
//! 2. Start the **lifeline**: the rest of stdin moves to a private fd watched by a thread, and
//!    fd 0 becomes `/dev/null` (libkrun takes over stdin for the guest console, which must not
//!    swallow the pipe). EOF = the owning process is gone → SIGKILL ourselves, VM included.
//!    On Linux `PR_SET_PDEATHSIG` is set as well.
//! 3. Raise the soft `RLIMIT_NOFILE` to the hard limit (see [`raise_nofile_limit`]).
//!    Verify pins and identities: the libkrunfw leaf-name lookup can only find the pinned
//!    file (74, see [`ffi::check_libkrunfw_lookup`]); libkrunfw + libkrun SHA-256 before
//!    `dlopen` (71 / 74); disks exist, optional disk SHA-256s (70 / 71); workspace roots'
//!    `(dev, ino)` against the lease identity (72). Shares are handed to libkrun as
//!    `/dev/fd/<n>` (Linux: `/proc/self/fd/<n>/.`) of the verified directory fd, closing the
//!    verify → open race (§8).
//!    Stale sockets at the vsock UDS paths (from a SIGKILLed run) are removed (70 if the
//!    path is something else).
//!    Network `userspace`: verify the pinned netstack's SHA-256 (71), spawn it on a
//!    socketpair behind the egress filter threads ([`netstack`]; 76 if it cannot start).
//! 4. Apply the [`ffi::plan`] (each `krun_*` call is logged to stderr first; 73 on failure).
//! 5. `krun_start_enter`: never returns on success (libkrun `exit()`s with the guest's
//!    status); returning is 75.
//!
//! stdout/stderr belong to the caller (the backend points them at `vmm.log`).

use std::io::{ErrorKind, Read};
use std::os::fd::{AsFd, AsRawFd, IntoRawFd, OwnedFd};
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use sandcrab_backend_libkrun::ffi::{self, FfiError, Krun, KrunCall};
use sandcrab_backend_libkrun::helper_proto::{
    HelperConfig, HelperNetwork, VirtioFsShare, exit_code,
};
use sandcrab_backend_libkrun::netstack::{self, EgressPolicy};

fn log(msg: impl std::fmt::Display) {
    eprintln!("sandcrab-krun: {msg}");
}

/// A pre-start failure: message + helper exit code.
struct Fail(i32, String);

impl Fail {
    fn new(code: i32, msg: impl std::fmt::Display) -> Self {
        Self(code, msg.to_string())
    }
}

fn main() {
    let Fail(code, msg) = run();
    log(format_args!(
        "error: {msg} (exit {code}: {})",
        exit_code::describe(code).unwrap_or("?")
    ));
    std::process::exit(code);
}

/// Only returns on failure; on success `krun_start_enter` takes over the process.
fn run() -> Fail {
    let t0 = Instant::now();
    let cfg = match HelperConfig::read_from(std::io::stdin().lock()) {
        Ok(c) => c,
        Err(e) => return Fail::new(exit_code::CONFIG_INVALID, e),
    };
    if let Err(e) = start_lifeline() {
        return Fail::new(exit_code::CONFIG_INVALID, format!("lifeline setup: {e}"));
    }

    raise_nofile_limit();

    if let Err(e) = ffi::check_libkrunfw_lookup(&cfg.libkrunfw.path, |k| std::env::var_os(k)) {
        return Fail::new(exit_code::LIBKRUN_LOAD, e);
    }
    let t_pins = Instant::now();
    let krun = match Krun::load(&cfg.libkrun, &cfg.libkrunfw) {
        Ok(k) => {
            log(format_args!(
                "libkrun + libkrunfw sha256 ok, loaded ({} ms)",
                t_pins.elapsed().as_millis()
            ));
            k
        }
        Err(e @ FfiError::ShaMismatch { .. }) => return Fail::new(exit_code::PIN_MISMATCH, e),
        Err(e) => return Fail::new(exit_code::LIBKRUN_LOAD, e),
    };
    if let Err(f) = verify_disks(&cfg) {
        return f;
    }
    if let Err(f) = clear_stale_sockets(&cfg) {
        return f;
    }
    // Opened, verified workspace roots; kept open until start_enter.
    let roots = match open_shares(&cfg.virtiofs) {
        Ok(r) => r,
        Err(f) => return f,
    };
    // `userspace` networking: the pinned netstack behind the egress filter. Its socket goes
    // to libkrun in the `krun_add_net_unixstream` call below.
    let mut nic = match start_netstack(&cfg, &krun) {
        Ok(fd) => fd,
        Err(f) => return f,
    };

    let config_err = |e: FfiError| Fail::new(exit_code::LIBKRUN_CONFIG, e);
    log(format_args!("krun_set_log_level({})", cfg.log_level));
    if let Err(e) = krun.set_log_level(cfg.log_level) {
        return config_err(e);
    }
    match krun.max_vcpus() {
        Ok(max) if u32::from(cfg.vcpus) > max => {
            return Fail::new(
                exit_code::LIBKRUN_CONFIG,
                format!("vcpus {} exceeds krun_get_max_vcpus() = {max}", cfg.vcpus),
            );
        }
        Ok(_) => {}
        Err(e) => return config_err(e),
    }
    let ctx = match krun.create_ctx() {
        Ok(c) => c,
        Err(e) => return config_err(e),
    };
    log(format_args!("krun_create_ctx() = {}", ctx.id()));

    let mut shares = roots.iter();
    for mut call in ffi::plan(&cfg) {
        if let KrunCall::AddVirtiofs { path, .. } = &mut call {
            // The share root the identity check just pinned (see `share_path`).
            let Some(root) = shares.next() else {
                return Fail::new(exit_code::LIBKRUN_CONFIG, "virtio-fs plan/config mismatch");
            };
            *path = root.share_path();
        }
        if let KrunCall::AddNetUnixstream { fd, .. } = &mut call {
            let Some(sock) = nic.take() else {
                return Fail::new(exit_code::LIBKRUN_CONFIG, "NIC planned without a netstack");
            };
            // libkrun owns the socket from here on (it is never closed by us).
            *fd = sock.into_raw_fd();
        }
        log(&call);
        if let Err(e) = ctx.apply(&call) {
            return config_err(e);
        }
    }

    log(format_args!(
        "krun_start_enter({}) after {} ms",
        ctx.id(),
        t0.elapsed().as_millis()
    ));
    let e = ctx.start_enter();
    drop(roots);
    Fail::new(exit_code::START_FAILED, e)
}

/// Raise the soft `RLIMIT_NOFILE` to the hard limit, as virtiofsd does. The VMM's virtio-fs
/// servers hold host fds on the guest's behalf: on Linux libkrun's passthrough keeps one
/// `O_PATH` fd per inode the guest has looked up (until the guest kernel forgets it), so
/// walking a few thousand files in a share exhausted the inherited soft limit (often 1024)
/// and the next guest create failed with `EMFILE`. macOS addresses inodes by `/.vol` path
/// (no per-inode fd) but still holds one per open handle, under a 256 default soft limit.
/// Best effort: a failure is logged and the VM still starts.
fn raise_nofile_limit() {
    use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};
    let cur = getrlimit(Resource::Nofile);
    let Some(target) = nofile_target(cur.current, cur.maximum, nofile_cap()) else {
        log(format_args!(
            "RLIMIT_NOFILE soft {} kept (hard {})",
            fmt_limit(cur.current),
            fmt_limit(cur.maximum)
        ));
        return;
    };
    let new = Rlimit {
        current: Some(target),
        maximum: cur.maximum,
    };
    match setrlimit(Resource::Nofile, new) {
        Ok(()) => log(format_args!(
            "RLIMIT_NOFILE soft {} -> {target} (hard {})",
            fmt_limit(cur.current),
            fmt_limit(cur.maximum)
        )),
        Err(e) => log(format_args!(
            "warning: raising RLIMIT_NOFILE soft {} -> {target}: {e}; large virtio-fs \
             shares may hit EMFILE",
            fmt_limit(cur.current)
        )),
    }
}

fn fmt_limit(l: Option<u64>) -> String {
    l.map_or_else(|| "unlimited".into(), |v| v.to_string())
}

/// Highest soft `RLIMIT_NOFILE` the kernel accepts. Linux: `fs.nr_open` (default 2^20), which
/// also bounds an unlimited hard limit.
#[cfg(target_os = "linux")]
fn nofile_cap() -> u64 {
    std::fs::read_to_string("/proc/sys/fs/nr_open")
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(1 << 20)
}

/// Highest soft `RLIMIT_NOFILE` the kernel accepts. macOS rejects `RLIM_INFINITY` and values
/// above `OPEN_MAX` for `setrlimit(RLIMIT_NOFILE)` (setrlimit(2) compatibility note).
#[cfg(not(target_os = "linux"))]
fn nofile_cap() -> u64 {
    10240 // OPEN_MAX, <sys/syslimits.h>
}

/// The soft limit to set: `min(hard, cap)` (`None` = unlimited), or `None` if the current soft
/// limit is already at least that (never lowers it).
fn nofile_target(soft: Option<u64>, hard: Option<u64>, cap: u64) -> Option<u64> {
    let target = hard.map_or(cap, |h| h.min(cap));
    match soft {
        Some(s) if s < target => Some(target),
        _ => None,
    }
}

/// Move the rest of stdin to a private fd watched by a thread; fd 0 becomes `/dev/null`.
fn start_lifeline() -> std::io::Result<()> {
    let pipe: OwnedFd = std::io::stdin().as_fd().try_clone_to_owned()?;
    let devnull = std::fs::File::open("/dev/null")?;
    rustix::stdio::dup2_stdin(&devnull)?;

    #[cfg(target_os = "linux")]
    let parent = rustix::process::getppid();

    std::thread::Builder::new()
        .name("lifeline".into())
        .spawn(move || {
            let mut pipe = std::fs::File::from(pipe);
            let mut buf = [0u8; 64];
            loop {
                match pipe.read(&mut buf) {
                    Ok(0) => break,
                    Ok(_) => {}
                    Err(e) if e.kind() == ErrorKind::Interrupted => {}
                    Err(_) => break,
                }
            }
            die("lifeline closed (owner gone)");
        })?;

    #[cfg(target_os = "linux")]
    {
        // Belt and braces with the pipe. If the parent already died before the prctl, the
        // signal never comes, so re-check the parent afterwards.
        rustix::process::set_parent_process_death_signal(Some(rustix::process::Signal::KILL))?;
        if rustix::process::getppid() != parent {
            die("parent exited before PR_SET_PDEATHSIG");
        }
    }
    Ok(())
}

/// SIGKILL ourselves (and so the VM, which lives in this process). No guest flush: the
/// owner is gone, nobody would read the result.
fn die(why: &str) -> ! {
    log(format_args!("{why}: killing the VM"));
    let _ = rustix::process::kill_process(rustix::process::getpid(), rustix::process::Signal::KILL);
    // Delivery to a multi-threaded process is asynchronous; give it time before the fallback.
    std::thread::sleep(std::time::Duration::from_secs(1));
    std::process::abort();
}

fn verify_disks(cfg: &HelperConfig) -> Result<(), Fail> {
    for d in [&cfg.root, &cfg.scratch].into_iter().chain(&cfg.disks) {
        match std::fs::metadata(&d.path) {
            Ok(m) if m.is_file() => {}
            Ok(_) => {
                return Err(Fail::new(
                    exit_code::CONFIG_INVALID,
                    format!("disk `{}` is not a regular file", d.path.display()),
                ));
            }
            Err(e) => {
                return Err(Fail::new(
                    exit_code::CONFIG_INVALID,
                    format!("disk `{}`: {e}", d.path.display()),
                ));
            }
        }
        if let Some(sha) = &d.sha256 {
            let t = Instant::now();
            match ffi::verify_sha256(&d.path, sha) {
                Ok(()) => log(format_args!(
                    "{} sha256 ok ({} ms)",
                    d.block_id,
                    t.elapsed().as_millis()
                )),
                Err(e @ FfiError::ShaMismatch { .. }) => {
                    return Err(Fail::new(exit_code::PIN_MISMATCH, e));
                }
                Err(e) => return Err(Fail::new(exit_code::CONFIG_INVALID, e)),
            }
        }
    }
    Ok(())
}

/// libkrun binds the vsock → UDS listeners itself and fails with `EEXIST` on a leftover
/// socket (e.g. from a SIGKILLed earlier helper on the same run dir). Remove it, but only if
/// it really is a socket: anything else at that path is a config error.
fn clear_stale_sockets(cfg: &HelperConfig) -> Result<(), Fail> {
    use std::os::unix::fs::FileTypeExt;
    for p in [&cfg.control_socket, &cfg.tools_socket] {
        match std::fs::symlink_metadata(p) {
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Ok(m) if m.file_type().is_socket() => {
                log(format_args!("removing stale socket `{}`", p.display()));
                std::fs::remove_file(p).map_err(|e| {
                    Fail::new(
                        exit_code::CONFIG_INVALID,
                        format!("removing stale socket `{}`: {e}", p.display()),
                    )
                })?;
            }
            Ok(_) => {
                return Err(Fail::new(
                    exit_code::CONFIG_INVALID,
                    format!("`{}` exists and is not a socket", p.display()),
                ));
            }
            Err(e) => {
                return Err(Fail::new(
                    exit_code::CONFIG_INVALID,
                    format!("`{}`: {e}", p.display()),
                ));
            }
        }
    }
    Ok(())
}

/// `userspace` networking (design §9 Phase B): verify the pinned netstack, spawn it on a
/// socketpair and start the egress filter threads. Returns the guest end of the NIC socket
/// (for `krun_add_net_unixstream`), or `None` for any other network mode.
///
/// The netstack runs as this (unprivileged) user, with an empty environment, in our process
/// group (and, on Linux, our cgroup leaf). It holds no listening socket: its only channel is
/// the socketpair, so it exits on EOF when this process dies, however it dies. A watcher
/// thread reaps it and logs its exit.
fn start_netstack(cfg: &HelperConfig, krun: &Krun) -> Result<Option<OwnedFd>, Fail> {
    if cfg.network != HelperNetwork::Userspace {
        return Ok(None);
    }
    let Some(spec) = &cfg.netstack else {
        return Err(Fail::new(
            exit_code::CONFIG_INVALID,
            "network `userspace` without a netstack",
        ));
    };
    if !krun.has_net() {
        return Err(Fail::new(
            exit_code::LIBKRUN_LOAD,
            format!(
                "this libkrun lacks {} (built without the `net` feature): reinstall it with \
                 scripts/fetch-libkrun.sh",
                ffi::ADD_NET_UNIXSTREAM
            ),
        ));
    }
    let bin = &spec.binary;
    let t = Instant::now();
    match ffi::verify_sha256(&bin.path, &bin.sha256) {
        Ok(()) => log(format_args!(
            "{} sha256 ok ({} ms)",
            spec.kind,
            t.elapsed().as_millis()
        )),
        Err(e @ FfiError::ShaMismatch { .. }) => {
            return Err(Fail::new(exit_code::PIN_MISMATCH, e));
        }
        Err(e) => return Err(Fail::new(exit_code::NETSTACK_START, e)),
    }
    let policy = EgressPolicy::new(&spec.deny);
    let host: Vec<String> = spec.deny.iter().map(ToString::to_string).collect();
    log(format_args!(
        "net: guest {}/{} gw+dns {} via {}; egress denied to {} built-in ranges + host [{}], \
         SMTP {:?}",
        netstack::GUEST_IP,
        netstack::SUBNET_PREFIX,
        netstack::GATEWAY_IP,
        spec.kind,
        netstack::ALWAYS_DENIED.len(),
        host.join(", "),
        netstack::SMTP_PORTS
    ));
    let logger: Arc<dyn Fn(String) + Send + Sync> = Arc::new(|m: String| log(m));
    let started = netstack::start(spec.kind, &bin.path, policy, logger).map_err(|e| {
        Fail::new(
            exit_code::NETSTACK_START,
            format!("spawning {} `{}`: {e}", spec.kind, bin.path.display()),
        )
    })?;
    let mut child = started.child;
    log(format_args!("{} started (pid {})", spec.kind, child.id()));
    let (kind, stats) = (spec.kind, started.stats);
    std::thread::Builder::new()
        .name("netstack-wait".into())
        .spawn(move || {
            let status = child.wait();
            log(format_args!(
                "net: {kind} exited ({}); frames to netstack {}, to guest {}, dropped {}",
                status.map_or_else(|e| e.to_string(), |s| s.to_string()),
                stats.to_netstack.load(Ordering::Relaxed),
                stats.to_guest.load(Ordering::Relaxed),
                stats.dropped.load(Ordering::Relaxed)
            ));
        })
        .map_err(|e| Fail::new(exit_code::NETSTACK_START, e))?;
    Ok(Some(started.guest_fd))
}

/// A workspace root opened `O_DIRECTORY | O_NOFOLLOW` and checked against its lease identity.
struct ShareRoot {
    fd: OwnedFd,
}

impl ShareRoot {
    /// Path handed to libkrun: `/dev/fd/<n>` of the verified directory fd, not the host path
    /// (design §8 identity TOCTOU). Opening `/dev/fd/<n>` duplicates the fd (macOS fdesc;
    /// Linux `/proc/self/fd` magic link), so libkrun's server gets exactly the inode whose
    /// `(dev, ino)` was checked, even if the path is swapped in between. libkrun opens the
    /// share root once while building the VM and works from that handle afterwards (verified
    /// on macOS: nested create/rename/symlink/chmod/unlink work, and a host rename+recreate of
    /// the path after boot stays invisible to the guest). The fd stays open (no exec happens,
    /// so `CLOEXEC` is irrelevant) until `krun_start_enter` takes over the process.
    ///
    /// **Linux:** libkrun's passthrough server opens the root with `O_PATH | O_NOFOLLOW`, and
    /// `/dev/fd/<n>` ends in the `/proc/self/fd/<n>` magic link, so `O_NOFOLLOW` would open
    /// the link itself (every guest access then fails with `EIO`). `/proc/self/fd/<n>/.`
    /// crosses the magic link as an intermediate component (always followed) and ends in `.`,
    /// the verified directory itself.
    fn share_path(&self) -> PathBuf {
        share_path_for(self.fd.as_raw_fd())
    }
}

#[cfg(target_os = "linux")]
fn share_path_for(fd: i32) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{fd}/."))
}

#[cfg(not(target_os = "linux"))]
fn share_path_for(fd: i32) -> PathBuf {
    PathBuf::from(format!("/dev/fd/{fd}"))
}

fn open_shares(shares: &[VirtioFsShare]) -> Result<Vec<ShareRoot>, Fail> {
    use rustix::fs::{Mode, OFlags};
    shares
        .iter()
        .map(|s| {
            let ident = |msg: String| Fail::new(exit_code::WORKSPACE_IDENTITY, msg);
            let fd = rustix::fs::open(
                &s.host_path,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(|e| ident(format!("opening `{}`: {e}", s.host_path.display())))?;
            if let Some(want) = s.identity {
                let file = std::fs::File::from(fd.try_clone().map_err(|e| ident(e.to_string()))?);
                let m = file
                    .metadata()
                    .map_err(|e| ident(format!("stat `{}`: {e}", s.host_path.display())))?;
                if (m.dev(), m.ino()) != (want.dev, want.ino) {
                    return Err(ident(format!(
                        "`{}` is ({}, {}), lease has ({}, {})",
                        s.host_path.display(),
                        m.dev(),
                        m.ino(),
                        want.dev,
                        want.ino
                    )));
                }
            }
            log(format_args!(
                "virtio-fs {:?} = {:?} verified, passed as fd {}",
                s.tag,
                s.host_path,
                fd.as_raw_fd()
            ));
            Ok(ShareRoot { fd })
        })
        .collect()
}

#[cfg(test)]
mod nofile_tests {
    use super::nofile_target;

    #[test]
    fn raises_soft_to_hard() {
        assert_eq!(
            nofile_target(Some(1024), Some(524_288), 1 << 20),
            Some(524_288)
        );
    }

    #[test]
    fn caps_at_the_kernel_maximum() {
        // Unlimited hard, and a hard above the cap (macOS: kern.maxfilesperproc > OPEN_MAX).
        assert_eq!(nofile_target(Some(256), None, 10_240), Some(10_240));
        assert_eq!(
            nofile_target(Some(256), Some(1 << 30), 10_240),
            Some(10_240)
        );
    }

    #[test]
    fn never_lowers() {
        assert_eq!(nofile_target(Some(524_288), Some(524_288), 1 << 20), None);
        assert_eq!(nofile_target(Some(20_000), None, 10_240), None);
        assert_eq!(nofile_target(None, None, 1 << 20), None);
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    /// The share path, opened the way libkrun's passthrough server opens its root
    /// (`O_PATH | O_NOFOLLOW`), is the verified directory, even after the host path is swapped.
    #[test]
    fn share_path_opens_the_pinned_directory() {
        use rustix::fs::{FileType, Mode, OFlags};
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("ws");
        std::fs::create_dir(&dir).unwrap();
        let fd = rustix::fs::open(
            &dir,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .unwrap();
        let want = std::fs::metadata(&dir).unwrap();
        std::fs::rename(&dir, tmp.path().join("moved")).unwrap();
        std::fs::create_dir(&dir).unwrap();

        let root = ShareRoot { fd };
        let opened = rustix::fs::open(
            root.share_path(),
            OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .unwrap();
        let st = rustix::fs::fstat(opened).unwrap();
        assert!(FileType::from_raw_mode(st.st_mode).is_dir());
        assert_eq!((st.st_dev, st.st_ino), (want.dev(), want.ino()));
    }
}
