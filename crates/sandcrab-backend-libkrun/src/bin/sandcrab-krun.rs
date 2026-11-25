//! `sandcrab-krun`: the per-sandbox libkrun VMM helper (design §2).
//!
//! 1. Read one [`HelperConfig`] JSON line from stdin and validate it (exit 70 on failure).
//! 2. Start the **lifeline**: the rest of stdin moves to a private fd watched by a thread, and
//!    fd 0 becomes `/dev/null` (libkrun takes over stdin for the guest console, which must not
//!    swallow the pipe). EOF = the owning process is gone → SIGKILL ourselves, VM included.
//!    This is the only owner-death mechanism: `PR_SET_PDEATHSIG` is deliberately not used,
//!    because it fires when the *thread* that forked the helper exits (a retired blocking-pool
//!    thread), not the owning process (review R8).
//! 3. Raise the soft `RLIMIT_NOFILE` to the hard limit (see [`raise_nofile_limit`]).
//!    Verify pins and identities, each through one fd that is then used instead of the path
//!    (design §12.4): libkrunfw + libkrun by stat stamp, else SHA-256 (71), `dlopen`ed through
//!    that fd, the leaf-name lookup of libkrunfw bound to the loaded image (74, see
//!    [`ffi::Krun::load`]); the root disk by stamp / SHA-256 (71), handed to libkrun as
//!    `/proc/self/fd/<n>` (macOS `/dev/fd/<n>`); other disks exist (70); workspace roots'
//!    `(dev, ino)` against the lease identity (72). Shares are handed to libkrun as
//!    `/dev/fd/<n>` (Linux: `/proc/self/fd/<n>/.`) of the verified directory fd, closing the
//!    verify → open race (§8).
//!    Stale sockets at the vsock UDS paths (from a SIGKILLed run) are removed (70 if the
//!    path is something else).
//!    Network `userspace`: verify the pinned netstack through an fd (71; passt: the pinned
//!    AVX2 variant on AVX2 CPUs), spawn it (Linux: `execve` of that fd) on a socketpair behind
//!    the egress filter threads ([`netstack`]; 76 if it cannot start).
//! 4. Apply the [`ffi::plan`] (each `krun_*` call is logged to stderr first; 73 on failure).
//! 5. `krun_start_enter`: never returns on success (libkrun `exit()`s with the guest's
//!    status); returning is 75.
//!
//! stdout/stderr belong to the caller (the backend points them at `vmm.log`).

use std::collections::HashMap;
use std::fs::File;
use std::io::{ErrorKind, Read};
use std::os::fd::{AsFd, AsRawFd, IntoRawFd, OwnedFd};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use sandcrab_backend_libkrun::ffi::{self, FfiError, Krun, KrunCall};
use sandcrab_backend_libkrun::helper_proto::{
    HelperConfig, HelperNetwork, NetstackSpec, PinnedFile, VirtioFsShare, exit_code,
};
use sandcrab_backend_libkrun::netstack::{self, EgressPolicy, NetstackKind};
use sandcrab_backend_libkrun::verified::{self, PinError};

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
    set_process_name();
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
            let how = |p: &PinnedFile| if p.stamp.is_some() { "stamp" } else { "sha256" };
            log(format_args!(
                "libkrun ({}) + libkrunfw ({}) verified, loaded by fd ({} ms)",
                how(&cfg.libkrun),
                how(&cfg.libkrunfw),
                t_pins.elapsed().as_millis()
            ));
            k
        }
        Err(e @ FfiError::ShaMismatch { .. }) => return Fail::new(exit_code::PIN_MISMATCH, e),
        Err(e) => return Fail::new(exit_code::LIBKRUN_LOAD, e),
    };
    // Verified disk fds by block id; kept open until start_enter.
    let disks = match open_disks(&cfg) {
        Ok(d) => d,
        Err(f) => return f,
    };
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
        if let KrunCall::AddDisk { block_id, path, .. } = &mut call
            && let Some(file) = disks.get(block_id.as_str())
        {
            // The image the stamp / SHA-256 check just verified, not the path.
            *path = verified::fd_path(file);
        }
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
    drop((roots, disks));
    Fail::new(exit_code::START_FAILED, e)
}

/// Linux: `comm` is the basename of the executed path, which is `<n>` when the backend
/// executes the helper through its verified fd (`/proc/self/fd/<n>`): name it back, so
/// `ps`/`pgrep` show `sandcrab-krun` (libkrun later renames its VMM threads itself).
fn set_process_name() {
    #[cfg(target_os = "linux")]
    if let Err(e) = rustix::thread::set_name(c"sandcrab-krun") {
        log(format_args!("warning: PR_SET_NAME: {e}"));
    }
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
///
/// The pipe's write end lives only in the owning process (the backend keeps it as the
/// lifeline; it is close-on-exec, so no other child inherits it), so EOF means exactly "the
/// owner process is gone", however it died, and whichever of its threads spawned us. That is
/// also why there is no `PR_SET_PDEATHSIG`: on Linux it fires when the forking *thread*
/// exits, so a helper spawned from a `spawn_blocking` / `block_in_place` thread was SIGKILLed
/// when that pool thread retired (review R8). If the owner died before this thread starts,
/// the first read already returns EOF.
fn start_lifeline() -> std::io::Result<()> {
    let pipe: OwnedFd = std::io::stdin().as_fd().try_clone_to_owned()?;
    let devnull = std::fs::File::open("/dev/null")?;
    rustix::stdio::dup2_stdin(&devnull)?;

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

/// Every disk must be a regular file. Disks with a stamp or a SHA-256 (the shared root) are
/// opened once and verified through that fd ([`verified::open_disk`]); the returned fds (by
/// block id) replace their paths in `krun_add_disk`, so libkrun opens the verified inode.
fn open_disks(cfg: &HelperConfig) -> Result<HashMap<String, File>, Fail> {
    let mut opened = HashMap::new();
    for d in [&cfg.root, &cfg.scratch].into_iter().chain(&cfg.disks) {
        let t = Instant::now();
        match verified::open_disk(d) {
            Ok(Some(file)) => {
                log(format_args!(
                    "{} verified ({}, {} ms), passed as fd {}",
                    d.block_id,
                    if d.stamp.is_some() { "stamp" } else { "sha256" },
                    t.elapsed().as_millis(),
                    file.as_raw_fd()
                ));
                opened.insert(d.block_id.clone(), file);
                continue;
            }
            Ok(None) => {}
            Err(PinError::Mismatch(m)) => return Err(Fail::new(exit_code::PIN_MISMATCH, m)),
            Err(PinError::Io(m)) => return Err(Fail::new(exit_code::CONFIG_INVALID, m)),
        }
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
    }
    Ok(opened)
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
    let bin = netstack_binary(spec, avx2_cpu())?;
    let t = Instant::now();
    let verified = match verified::open_pinned(bin) {
        Ok(v) => {
            log(format_args!(
                "{} `{}` verified ({:?}, {} ms)",
                spec.kind,
                bin.path.display(),
                v.how,
                t.elapsed().as_millis()
            ));
            v.file
        }
        Err(PinError::Mismatch(m)) => return Err(Fail::new(exit_code::PIN_MISMATCH, m)),
        Err(PinError::Io(m)) => return Err(Fail::new(exit_code::NETSTACK_START, m)),
    };
    let exec = exec_path(&verified, &bin.path);
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
    let started = netstack::start(spec.kind, &exec, &bin.path, policy, logger).map_err(|e| {
        Fail::new(
            exit_code::NETSTACK_START,
            format!("spawning {} `{}`: {e}", spec.kind, bin.path.display()),
        )
    })?;
    drop(verified);
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

/// What to execute for a verified file: Linux `/proc/self/fd/<n>` (the kernel resolves it in
/// the child before close-on-exec fds are closed, so the verified inode is what runs). macOS
/// cannot `exec` an fdesc path, so it executes the path right after the check; that residual
/// window is documented in design §12.4.
fn exec_path(file: &File, path: &Path) -> PathBuf {
    if cfg!(target_os = "linux") {
        verified::fd_path(file)
    } else {
        path.to_path_buf()
    }
}

/// Does passt's `__builtin_cpu_supports("avx2")` hold here (CPUID + OS-enabled AVX state)?
fn avx2_cpu() -> bool {
    #[cfg(target_arch = "x86_64")]
    return std::arch::is_x86_feature_detected!("avx2");
    #[cfg(not(target_arch = "x86_64"))]
    false
}

/// The netstack file to execute. On x86_64 passt re-execs `<readlink /proc/self/exe>.avx2`
/// **by path** when the CPU has AVX2 and its own name does not end in `.avx2` (passt
/// `arch.c`), which would run an unverified file. So on AVX2 CPUs the helper executes the
/// pinned `passt.avx2` itself (passt then does not re-exec); without a pinned variant (an older
/// install) it refuses to start plain passt while a `.avx2` sibling exists, and otherwise
/// passt's re-exec finds nothing and it keeps running the verified binary.
fn netstack_binary(spec: &NetstackSpec, avx2: bool) -> Result<&PinnedFile, Fail> {
    if spec.kind != NetstackKind::Passt || !avx2 {
        return Ok(&spec.binary);
    }
    if let Some(v) = &spec.avx2 {
        return Ok(v);
    }
    let mut sibling = spec.binary.path.clone().into_os_string();
    sibling.push(".avx2");
    let sibling = PathBuf::from(sibling);
    if std::fs::symlink_metadata(&sibling).is_ok() {
        return Err(Fail::new(
            exit_code::PIN_MISMATCH,
            format!(
                "`{}` exists but is not pinned: passt would execute it unverified on this AVX2 \
                 CPU; reinstall the netstack (`sandcrab libkrun install --force`)",
                sibling.display()
            ),
        ));
    }
    Ok(&spec.binary)
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
mod netstack_tests {
    use super::*;

    fn spec(dir: &Path, kind: NetstackKind, avx2: bool) -> NetstackSpec {
        let pin = |name: &str| PinnedFile::new(dir.join(name), "0".repeat(64));
        NetstackSpec {
            kind,
            binary: pin("passt"),
            avx2: avx2.then(|| pin("passt.avx2")),
            deny: vec![],
        }
    }

    #[test]
    fn avx2_cpus_run_the_pinned_variant() {
        let tmp = tempfile::tempdir().unwrap();
        let s = spec(tmp.path(), NetstackKind::Passt, true);
        let bin = |avx2| netstack_binary(&s, avx2).ok().map(|p| p.path.clone());
        assert_eq!(bin(true), Some(tmp.path().join("passt.avx2")));
        assert_eq!(bin(false), Some(tmp.path().join("passt")));
    }

    #[test]
    fn an_unpinned_avx2_sibling_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let s = spec(tmp.path(), NetstackKind::Passt, false);
        // Nothing next to it: plain passt (its re-exec finds nothing).
        assert!(netstack_binary(&s, true).is_ok());
        std::fs::write(tmp.path().join("passt.avx2"), b"planted").unwrap();
        let Err(Fail(code, msg)) = netstack_binary(&s, true) else {
            panic!("an unpinned passt.avx2 must be refused");
        };
        assert_eq!(code, exit_code::PIN_MISMATCH);
        assert!(msg.contains("not pinned"), "{msg}");
        // No AVX2: passt never looks for it.
        assert!(netstack_binary(&s, false).is_ok());
        // gvproxy has no such re-exec.
        let g = spec(tmp.path(), NetstackKind::Gvproxy, false);
        assert!(netstack_binary(&g, true).is_ok());
    }
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
