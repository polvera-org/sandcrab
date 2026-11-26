//! `crabd init`: the PID 1 boot sequence and the real [`InitOps`] (Linux only).
//!
//! Order (design §3): early mounts → overlay root + `pivot_root` → late mounts → hostname,
//! `lo` up, Ctrl-Alt-Del off → pids cgroup → hardening sysctls, read-only `/proc/sys` (and
//! Docker's other `ReadonlyPaths`) and `/sys` → reaper → control port. `init.configure` then
//! applies its `/tmp`, `pids_max` and `nofile` options, mounts the workspaces and starts
//! `crabd serve` as the sandbox user.

use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    process::ExitCode,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU32, Ordering},
    },
    time::Duration,
};

use sandcrab_protocol::init::{
    CONTROL_PORT, DEFAULT_SHUTDOWN_GRACE_MS, InitConfig, InitConfigureResult, PROC_READONLY_PATHS,
    SANDBOX_PIDS_MAX, SCRATCH_DRIVE_ID,
};
use tokio::signal::unix::{SignalKind, signal};
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

use super::{
    command::serve_command,
    config::{self, MountStep, StepSource, ValidatedConfig},
    control::{InitOps, serve_control_connection},
    dirs,
    drives::{self, BlockDevice},
    netlink, reap, sys, sysctl,
};
use crate::{hardening, server::BoxFuture, vsock};

/// Held while spawning children and while reaping, so the reaper can never collect a child
/// whose exec failed before `std` collects it (std asserts on that `waitpid`).
static SPAWN_LOCK: Mutex<()> = Mutex::new(());

/// Entry point of PID 1. Never returns in a healthy guest: it ends by rebooting.
pub async fn run() -> ExitCode {
    let early = sys::early_mounts();
    init_logging();
    info!(version = crate::CRABD_VERSION, "crabd init starting");
    if let Err(e) = early {
        error!(error = %e, "early mounts failed");
    }
    if let Err(e) = boot().await {
        error!(error = %e, "crabd init failed; rebooting");
    }
    sys::sync_and_unmount(&[]);
    let e = sys::reboot_now();
    error!(error = %e, "reboot failed");
    // Exiting PID 1 panics the kernel; with `panic=1` that still ends the VM.
    ExitCode::FAILURE
}

/// Logs go to the console (serial, when enabled with `--debug-console`), else `/dev/kmsg`.
fn init_logging() {
    let open = |p: &str| fs::OpenOptions::new().write(true).open(p);
    let sink: Box<dyn Write + Send> = match open("/dev/console").or_else(|_| open("/dev/kmsg")) {
        Ok(f) => Box::new(f),
        Err(_) => Box::new(io::stderr()),
    };
    let _ = tracing_subscriber::fmt()
        .with_writer(Mutex::new(sink))
        .with_ansi(false)
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .try_init();
}

async fn boot() -> io::Result<()> {
    let devices = drives::scan_block_devices(Path::new("/sys/block"), Path::new("/dev"))?;
    info!(?devices, "block devices");
    match resolve(&devices, SCRATCH_DRIVE_ID, SCRATCH_DRIVE_ID) {
        Some(dev) => sys::overlay_root(&dev)?,
        None => warn!("no scratch drive; the root filesystem stays read-only"),
    }
    sys::late_mounts()?;
    if let Err(e) = sys::set_hostname("sandcrab") {
        warn!(error = %e, "hostname");
    }
    if let Err(e) = netlink::link_up("lo") {
        warn!(error = %e, "could not bring up lo");
    }
    if let Err(e) = sys::disable_cad() {
        warn!(error = %e, "could not disable Ctrl-Alt-Del");
    }
    if let Err(e) = sys::disable_core_dumps() {
        warn!(error = %e, "could not disable core dumps");
    }
    let pids_cap = pids_cap();
    let hardening = harden_kernel();
    if !hardening.is_empty() {
        // Keep booting so the host gets this as a clear `init.configure` error instead of a
        // silent boot timeout; no sandbox code ever starts (see `hardening_refusal`).
        error!(failed = ?hardening, "guest hardening failed; init.configure will refuse");
    }

    let ops = Arc::new(Pid1Ops::new(devices, pids_cap, hardening));
    let mut sigchld = signal(SignalKind::child())?;
    let mut sigint = signal(SignalKind::interrupt())?;
    let listener = vsock::bind(CONTROL_PORT)?;
    info!(
        port = CONTROL_PORT,
        uptime_ms = sys::uptime_ms(),
        "control port listening"
    );
    reap(&ops.serve_pid);

    loop {
        tokio::select! {
            _ = sigchld.recv() => reap(&ops.serve_pid),
            _ = sigint.recv() => {
                info!("Ctrl-Alt-Del: shutting down");
                ops.do_shutdown(Duration::from_millis(DEFAULT_SHUTDOWN_GRACE_MS)).await;
            }
            accepted = listener.accept() => match accepted {
                Ok((stream, peer)) => {
                    let ops: Arc<dyn InitOps> = Arc::clone(&ops) as _;
                    tokio::spawn(async move {
                        if let Err(e) = serve_control_connection(stream, ops).await {
                            warn!(%peer, error = %e, "control connection failed");
                        }
                    });
                }
                Err(e) => {
                    warn!(error = %e, "control accept failed");
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            },
        }
    }
}

/// How the sandbox's task count is capped (Docker's `pids_limit`, [`SANDBOX_PIDS_MAX`]).
#[derive(Debug)]
enum PidsCap {
    /// cgroup v2 `pids.max` on this cgroup; `crabd serve` is spawned into it.
    Cgroup(PathBuf),
    /// No `pids` controller: `RLIMIT_NPROC` on the sandbox uid (not enforced for uid 0).
    Rlimit,
    /// Neither could be set up.
    Unlimited,
}

/// cgroup v2 `pids.max` is the primary cap: it counts every task in the subtree whatever its
/// uid (uid 0 included), threads too, as Docker's pids cgroup does, and the sandbox cannot
/// leave or raise it. `RLIMIT_NPROC` only if the kernel lacks the `pids` controller.
fn pids_cap() -> PidsCap {
    match sys::setup_pids_cgroup(SANDBOX_PIDS_MAX) {
        Ok(dir) => {
            info!(cgroup = %dir.display(), pids_max = SANDBOX_PIDS_MAX, "sandbox pids cap");
            PidsCap::Cgroup(dir)
        }
        Err(e) => {
            warn!(error = %e, "no pids cgroup; falling back to RLIMIT_NPROC (not applied to uid 0)");
            match sys::limit_nproc(SANDBOX_PIDS_MAX) {
                Ok(()) => PidsCap::Rlimit,
                Err(e) => {
                    error!(error = %e, "no fork-bomb cap for the sandbox");
                    PidsCap::Unlimited
                }
            }
        }
    }
}

/// Kernel-interface hardening, before any sandbox code can run (`crabd serve` only starts at
/// `init.configure`): the [`sysctl::HARDENING_SYSCTLS`], then Docker's read-only `/proc`
/// paths (so a uid-0 sandbox user cannot write the sysctls back through the owner bits) and
/// a read-only `/sys`.
///
/// **Fails closed** (`docs/design/guest-seccomp.md`): returns every step that failed, and
/// `init.configure` then refuses to start the sandbox. A sysctl the kernel does not have
/// (`ENOENT`, [`sysctl::Outcome::Missing`]) is not a failure: the guest kernel is pinned, and
/// a missing key means the feature it guards is missing too.
fn harden_kernel() -> Vec<String> {
    let mut failed = Vec::new();
    let mut set = Vec::new();
    let mut missing = Vec::new();
    for (s, outcome) in sysctl::apply_all(Path::new(sysctl::PROC_SYS), sysctl::HARDENING_SYSCTLS) {
        match outcome {
            sysctl::Outcome::Set(v) => set.push(format!("{}={v}", s.key)),
            sysctl::Outcome::Missing => missing.push(s.key),
            sysctl::Outcome::Failed(e) => {
                error!(key = s.key, error = %e, "sysctl not hardened");
                failed.push(format!("sysctl {}: {e}", s.key));
            }
        }
    }
    info!(?set, "hardening sysctls");
    if !missing.is_empty() {
        info!(?missing, "sysctls not supported by this kernel");
    }
    for p in PROC_READONLY_PATHS {
        if let Err(e) = sys::seal_readonly_path(Path::new(p)) {
            error!(error = %e, "stays writable");
            failed.push(format!("read-only {p}: {e}"));
        }
    }
    if let Err(e) = sys::seal_sysfs() {
        error!(error = %e, "/sys stays writable");
        failed.push(format!("read-only /sys: {e}"));
    }
    failed
}

/// Why `init.configure` must refuse to start sandbox code, if it must: any failed hardening
/// step, no fork-bomb cap at all, or a uid-0 sandbox user with only the `RLIMIT_NPROC`
/// fallback (the kernel never applies it to uid 0).
fn hardening_refusal(failed: &[String], pids_cap: &PidsCap, uid: u32) -> Option<String> {
    let mut why = failed.to_vec();
    match pids_cap {
        PidsCap::Cgroup(_) => {}
        PidsCap::Rlimit if uid == 0 => {
            why.push("no pids cgroup, and RLIMIT_NPROC does not apply to uid 0".into());
        }
        PidsCap::Rlimit => {}
        PidsCap::Unlimited => why.push("no pids cap (neither cgroup nor RLIMIT_NPROC)".into()),
    }
    (!why.is_empty()).then(|| {
        format!(
            "guest hardening failed, refusing to start the sandbox: {}",
            why.join("; ")
        )
    })
}

fn resolve(devices: &[BlockDevice], drive_id: &str, label: &str) -> Option<PathBuf> {
    drives::resolve_drive(devices, drive_id, label, |p| {
        drives::read_ext4_label(p).ok().flatten()
    })
}

/// Reap every exited child: orphans re-parented to PID 1 and `crabd serve` itself (see
/// [`super::reap`] for why a global `waitpid(-1)` cannot steal anyone's exit status).
fn reap(serve_pid: &AtomicU32) {
    let _guard = SPAWN_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    reap::reap_exited(|pid, status| {
        if pid == serve_pid.load(Ordering::Relaxed) {
            error!(pid, ?status, "crabd serve exited; tools are unavailable");
            serve_pid.store(0, Ordering::Relaxed);
        }
    });
}

struct Configured {
    mounts: Vec<MountStep>,
}

struct Pid1Ops {
    devices: Vec<BlockDevice>,
    pids_cap: PidsCap,
    /// Hardening steps that failed at boot ([`harden_kernel`]).
    hardening_failed: Vec<String>,
    state: tokio::sync::Mutex<Option<Configured>>,
    serve_pid: AtomicU32,
}

impl Pid1Ops {
    fn new(devices: Vec<BlockDevice>, pids_cap: PidsCap, hardening_failed: Vec<String>) -> Self {
        Self {
            devices,
            pids_cap,
            hardening_failed,
            state: tokio::sync::Mutex::new(None),
            serve_pid: AtomicU32::new(0),
        }
    }

    async fn do_configure(&self, cfg: InitConfig) -> io::Result<InitConfigureResult> {
        let mut state = self.state.lock().await;
        if state.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "already configured",
            ));
        }
        let v = config::validate_config(cfg)?;
        let c = &v.config;
        info!(sandbox = %c.sandbox_id, mounts = v.mounts.len(), "init.configure");
        // Fail closed before touching anything: no workspace is mounted, no sandbox code runs.
        if let Some(why) = hardening_refusal(&self.hardening_failed, &self.pids_cap, c.user.uid) {
            error!(uid = c.user.uid, "{why}");
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, why));
        }
        if let Some(ms) = c.host_time_unix_ms {
            sys::set_clock(ms).unwrap_or_else(|e| warn!(error = %e, "clock"));
        }
        sys::set_hostname(&c.hostname)?;
        if c.network && !c.dns.is_empty() {
            fs::write("/etc/resolv.conf", config::resolv_conf(&c.dns))?;
        }
        // Fail closed: an option that cannot be applied refuses the sandbox.
        if let Some(tmp) = v.tmp_remount() {
            sys::remount_tmp(tmp)?;
            info!(exec = tmp.exec, size_mb = tmp.size_mb, "/tmp remounted");
        }
        self.apply_pids_max(v.pids_max)?;
        let mut mounted = Vec::new();
        for step in &v.mounts {
            let source = match &step.source {
                StepSource::Block { drive_id, label } => resolve(&self.devices, drive_id, label)
                    .ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::NotFound,
                            format!("no block device for drive `{drive_id}`"),
                        )
                    })?
                    .to_string_lossy()
                    .into_owned(),
                StepSource::VirtioFs { tag } => tag.clone(),
            };
            // Never follow a symlink to the mount point: it may sit inside an earlier,
            // persistent workspace.
            let dir = dirs::ensure_dir_in(Path::new("/"), &step.target)?;
            sys::mount_workspace(&source, &dir.fd, step)?;
            info!(source = %source, fstype = step.fstype(), target = %step.target.display(), ro = step.read_only, "workspace mounted");
            mounted.push(step.clone());
        }
        sys::create_owned_dir(&v.default_cwd, c.user.uid, c.user.gid)?;
        let pid = self.spawn_serve(&v)?;
        *state = Some(Configured { mounts: mounted });
        Ok(InitConfigureResult {
            serve_pid: pid,
            boot_ms: sys::uptime_ms(),
        })
    }

    /// Set the configured `pids_max` on the cap made at boot (with [`SANDBOX_PIDS_MAX`]).
    /// The cgroupfs is still writable here: it is sealed after `crabd serve` is placed.
    fn apply_pids_max(&self, pids_max: u32) -> io::Result<()> {
        if pids_max == SANDBOX_PIDS_MAX {
            return Ok(());
        }
        match &self.pids_cap {
            PidsCap::Cgroup(dir) => sys::set_pids_max(dir, pids_max)?,
            PidsCap::Rlimit => sys::limit_nproc(pids_max)?,
            // `hardening_refusal` already refused the sandbox.
            PidsCap::Unlimited => return Ok(()),
        }
        info!(pids_max, "sandbox pids cap set");
        Ok(())
    }

    /// Spawn `crabd serve` as the sandbox user with an empty capability bounding set, inside
    /// the pids cgroup.
    ///
    /// Both without `pre_exec` (which would need `unsafe`): the fork happens on a dedicated
    /// thread whose bounding set was cleared first (it is per thread and copied into the
    /// child; PID 1's other threads keep theirs), and PID 1 briefly enters the sandbox cgroup
    /// so the child is born in it (cgroup v2 moves the whole process), then moves back. The
    /// child never runs uncapped, not even for an instant.
    fn spawn_serve(&self, v: &ValidatedConfig) -> io::Result<u32> {
        let mut cmd = serve_command(Path::new("/proc/self/exe"), v);
        let cgroup = match &self.pids_cap {
            PidsCap::Cgroup(dir) => Some(dir.as_path()),
            PidsCap::Rlimit | PidsCap::Unlimited => None,
        };
        let _guard = SPAWN_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let child = std::thread::scope(|s| {
            s.spawn(|| {
                // Fail closed: never start tools with a non-empty bounding set.
                hardening::clear_capability_bounding_set()?;
                let old_nofile = v.config.nofile.map(sys::set_nofile).transpose()?;
                let child = match cgroup {
                    None => cmd.spawn(),
                    Some(dir) => sys::enter_cgroup(dir).and_then(|()| {
                        let child = cmd.spawn();
                        if let Err(e) = sys::enter_cgroup(Path::new(sys::CGROUP_ROOT)) {
                            error!(error = %e, "PID 1 could not leave the sandbox cgroup");
                        }
                        child
                    }),
                };
                if let Some(old) = old_nofile
                    && let Err(e) = sys::restore_nofile(old)
                {
                    warn!(error = %e, "PID 1 keeps the sandbox's RLIMIT_NOFILE");
                }
                child
            })
            .join()
            .map_err(|_| io::Error::other("spawn thread panicked"))?
        })?;
        if cgroup.is_some()
            && let Err(e) = sys::seal_cgroupfs()
        {
            // Fail closed: a writable cgroupfs lets the sandbox raise `pids.max` or leave the
            // cgroup. `crabd serve` has not been handed a connection yet (the host only
            // connects after `init.configure` succeeds), so kill it before it is used.
            let mut child = child;
            let _ = child.kill();
            let why = format!(
                "guest hardening failed, refusing to start the sandbox: read-only cgroupfs: {e}"
            );
            error!("{why}");
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, why));
        }
        // The `Child` is dropped without `wait`: the reaper owns all exit statuses.
        let pid = child.id();
        self.serve_pid.store(pid, Ordering::Relaxed);
        info!(
            pid,
            uid = v.config.user.uid,
            gid = v.config.user.gid,
            port = v.config.tools_port,
            pids_cap = ?self.pids_cap,
            pids_max = v.pids_max,
            nofile = ?v.config.nofile,
            forward_ports = ?v.config.forward_ports,
            "crabd serve started"
        );
        Ok(pid)
    }

    async fn do_shutdown(&self, grace: Duration) {
        info!(grace_ms = grace.as_millis(), "init.shutdown");
        sys::terminate_all(grace).await;
        reap(&self.serve_pid);
        let targets: Vec<PathBuf> = self
            .state
            .lock()
            .await
            .as_ref()
            .map(|c| c.mounts.iter().map(|m| m.target.clone()).collect())
            .unwrap_or_default();
        sys::sync_and_unmount(&targets);
        let e = sys::reboot_now();
        error!(error = %e, "reboot failed");
    }
}

impl InitOps for Pid1Ops {
    fn configure(self: Arc<Self>, cfg: InitConfig) -> BoxFuture<io::Result<InitConfigureResult>> {
        Box::pin(async move { self.do_configure(cfg).await })
    }

    fn export_root(&self, drive_id: &str) -> io::Result<PathBuf> {
        let state = self
            .state
            .try_lock()
            .map_err(|_| io::Error::other("configuration in progress"))?;
        let c = state
            .as_ref()
            .ok_or_else(|| io::Error::other("not configured"))?;
        let m = c
            .mounts
            .iter()
            .find(|m| m.drive_id() == Some(drive_id))
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("drive `{drive_id}` is not mounted"),
                )
            })?;
        if m.read_only {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("drive `{drive_id}` is read-only; nothing to export"),
            ));
        }
        Ok(m.target.clone())
    }

    fn shutdown(self: Arc<Self>, grace: Duration) -> BoxFuture<()> {
        Box::pin(async move { self.do_shutdown(grace).await })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hardening_fails_closed() {
        let cg = PidsCap::Cgroup(PathBuf::from("/sys/fs/cgroup/sandbox"));
        // All good, any uid.
        for uid in [0, 1000] {
            assert_eq!(hardening_refusal(&[], &cg, uid), None);
        }
        // A failed mandatory step refuses for every uid, and names the step.
        let failed = vec!["read-only /proc/sys: EPERM".to_owned()];
        for uid in [0, 1000] {
            let why = hardening_refusal(&failed, &cg, uid).unwrap();
            assert!(
                why.contains("refusing") && why.contains("/proc/sys"),
                "{why}"
            );
        }
        // The RLIMIT_NPROC fallback is enough for a non-root user, never for uid 0.
        assert_eq!(hardening_refusal(&[], &PidsCap::Rlimit, 1000), None);
        let why = hardening_refusal(&[], &PidsCap::Rlimit, 0).unwrap();
        assert!(why.contains("uid 0"), "{why}");
        // No cap at all refuses everyone.
        assert!(hardening_refusal(&[], &PidsCap::Unlimited, 1000).is_some());
    }
}
