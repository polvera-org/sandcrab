//! `crabd init`: the PID 1 boot sequence and the real [`InitOps`] (Linux only).
//!
//! Order (design §3): early mounts → overlay root + `pivot_root` → late mounts → hostname,
//! `lo` up, Ctrl-Alt-Del off → reaper → control port. `init.configure` then mounts the
//! workspaces and starts `crabd serve` as the sandbox user.

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

use rustix::process::WaitOptions;
use sandcrab_protocol::init::{
    CONTROL_PORT, DEFAULT_SHUTDOWN_GRACE_MS, InitConfig, InitConfigureResult, SCRATCH_DRIVE_ID,
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
    netlink, sys,
};
use crate::{server::BoxFuture, vsock};

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

    let ops = Arc::new(Pid1Ops::new(devices));
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

fn resolve(devices: &[BlockDevice], drive_id: &str, label: &str) -> Option<PathBuf> {
    drives::resolve_drive(devices, drive_id, label, |p| {
        drives::read_ext4_label(p).ok().flatten()
    })
}

/// Reap every exited child: orphans re-parented to PID 1 and `crabd serve` itself.
fn reap(serve_pid: &AtomicU32) {
    let _guard = SPAWN_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    loop {
        match rustix::process::waitpid(None, WaitOptions::NOHANG) {
            Ok(Some((pid, status))) => {
                let pid = pid.as_raw_nonzero().get().unsigned_abs();
                if pid == serve_pid.load(Ordering::Relaxed) {
                    error!(pid, ?status, "crabd serve exited; tools are unavailable");
                    serve_pid.store(0, Ordering::Relaxed);
                }
            }
            Ok(None) => break,
            Err(rustix::io::Errno::CHILD) => break,
            Err(rustix::io::Errno::INTR) => {}
            Err(e) => {
                warn!(error = %e, "waitpid");
                break;
            }
        }
    }
}

struct Configured {
    mounts: Vec<MountStep>,
}

struct Pid1Ops {
    devices: Vec<BlockDevice>,
    state: tokio::sync::Mutex<Option<Configured>>,
    serve_pid: AtomicU32,
}

impl Pid1Ops {
    fn new(devices: Vec<BlockDevice>) -> Self {
        Self {
            devices,
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
        if let Some(ms) = c.host_time_unix_ms {
            sys::set_clock(ms).unwrap_or_else(|e| warn!(error = %e, "clock"));
        }
        sys::set_hostname(&c.hostname)?;
        if c.network && !c.dns.is_empty() {
            fs::write("/etc/resolv.conf", config::resolv_conf(&c.dns))?;
        }
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

    fn spawn_serve(&self, v: &ValidatedConfig) -> io::Result<u32> {
        let mut cmd = serve_command(Path::new("/proc/self/exe"), v);
        let _guard = SPAWN_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // The `Child` is dropped without `wait`: the reaper owns all exit statuses.
        let child = cmd.spawn()?;
        let pid = child.id();
        self.serve_pid.store(pid, Ordering::Relaxed);
        info!(
            pid,
            uid = v.config.user.uid,
            gid = v.config.user.gid,
            port = v.config.tools_port,
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
