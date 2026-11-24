//! One Firecracker microVM: launch, configure, boot, and teardown (design §4, §7, §9).

use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use sandcrab_core::{
    Connector, LifecycleTimings, PortForwards, PublishedPort, Result, SandboxError, SandboxHandle,
    SandboxId,
};
use sandcrab_protocol::init::{CONTROL_PORT, InitConfig, InitConfigureResult};
use tokio::process::{Child, Command};

use crate::api::{
    ActionType, ApiClient, BootSource, Drive, IoEngine, MachineConfig, NetworkInterface, Vsock,
};
use crate::config::FirecrackerConfig;
use crate::control::connect_control;
use crate::net::TapLease;
use crate::proc::ProcId;
use crate::rundir::{self, API_SOCK, CgroupLeaf, LOG_FILE, RunMeta, RwDriveMeta, VSOCK_SOCK};
use crate::vsock::VsockConnector;
use crate::workspace;

pub(crate) const BACKEND: &str = "firecracker";
/// Guest CID (any value ≥ 3; each VM has its own vsock device).
pub const GUEST_CID: u32 = 3;

pub(crate) fn fc_err(context: &str, err: impl std::fmt::Display) -> SandboxError {
    SandboxError::Backend {
        backend: BACKEND.into(),
        message: format!("{context}: {err}"),
    }
}

/// Everything a running (or half-started) VM owns. Dropping it without
/// [`Vm::shutdown`] kills the VMM and cleans up best-effort (rw images are preserved for
/// recovery).
pub(crate) struct Vm {
    pub id: SandboxId,
    pub dir: PathBuf,
    pub meta: RunMeta,
    pub child: Option<Child>,
    pub vmm: Option<ProcId>,
    pub tap: Option<TapLease>,
    pub cgroup: Option<CgroupLeaf>,
    pub recovery_dir: PathBuf,
    /// Set once cleanup ran (or ownership moved), so `Drop` does nothing.
    pub done: bool,
}

impl std::fmt::Debug for Vm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Vm")
            .field("id", &self.id)
            .field("dir", &self.dir)
            .field("vmm", &self.vmm)
            .field("tap", &self.tap.as_ref().map(|t| &t.plan.name))
            .finish_non_exhaustive()
    }
}

impl Vm {
    pub fn new(id: SandboxId, dir: PathBuf, meta: RunMeta, recovery_dir: PathBuf) -> Self {
        Self {
            id,
            dir,
            meta,
            child: None,
            vmm: None,
            tap: None,
            cgroup: None,
            recovery_dir,
            done: false,
        }
    }

    pub fn api_sock(&self) -> PathBuf {
        self.dir.join(API_SOCK)
    }

    pub fn vsock(&self) -> PathBuf {
        self.dir.join(VSOCK_SOCK)
    }

    /// Launch `firecracker` (own process group, stdio → `firecracker.log`, empty env).
    pub fn launch(&mut self, cfg: &FirecrackerConfig) -> Result<()> {
        let log_path = self.dir.join(LOG_FILE);
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .map_err(|e| workspace::io_at(&log_path, e))?;
        let log2 = log
            .try_clone()
            .map_err(|e| workspace::io_at(&log_path, e))?;
        let mut cmd = Command::new(&cfg.firecracker_bin);
        cmd.arg("--api-sock")
            .arg(self.api_sock())
            .arg("--id")
            .arg(self.id.to_string())
            .arg("--level")
            .arg(&cfg.log_level)
            .current_dir(&self.dir)
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(log2))
            .process_group(0)
            .kill_on_drop(true);
        let child = cmd.spawn().map_err(|e| {
            fc_err(
                &format!("cannot launch `{}`", cfg.firecracker_bin.display()),
                e,
            )
        })?;
        let pid = child
            .id()
            .ok_or_else(|| fc_err("firecracker", "exited immediately"))?;
        self.vmm = Some(ProcId::of(pid).unwrap_or(ProcId {
            pid,
            starttime: None,
        }));
        self.child = Some(child);
        self.meta.vmm = self.vmm;
        self.meta.save(&self.dir)
    }

    /// Exit status if the VMM has exited (non-blocking).
    fn exited(&mut self) -> Option<String> {
        let child = self.child.as_mut()?;
        match child.try_wait() {
            Ok(Some(status)) => Some(status.to_string()),
            _ => None,
        }
    }

    fn with_log(&self, msg: String) -> String {
        let tail = rundir::log_tail(&self.dir, 2048);
        if tail.is_empty() {
            msg
        } else {
            format!("{msg}; firecracker.log: {tail}")
        }
    }

    /// Poll `GET /` until the API socket answers.
    pub async fn wait_api(&mut self, api: &mut ApiClient, deadline: Instant) -> Result<()> {
        let started = Instant::now();
        let mut backoff = Duration::from_millis(1);
        loop {
            if self.dir.join(API_SOCK).exists() {
                match api.instance_info().await {
                    Ok(_) => return Ok(()),
                    Err(e) if Instant::now() >= deadline => {
                        return Err(fc_err(
                            "API socket never answered",
                            self.with_log(e.to_string()),
                        ));
                    }
                    Err(_) => {}
                }
            }
            if let Some(status) = self.exited() {
                return Err(fc_err(
                    "firecracker exited during startup",
                    self.with_log(status),
                ));
            }
            if Instant::now() >= deadline {
                return Err(SandboxError::Timeout {
                    what: self.with_log("firecracker API socket".into()),
                    after: started.elapsed(),
                });
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_millis(10));
        }
    }

    /// Wait up to `d` for the VMM to exit; true if it did.
    async fn wait_exit(&mut self, d: Duration) -> bool {
        let Some(child) = self.child.as_mut() else {
            return true;
        };
        matches!(tokio::time::timeout(d, child.wait()).await, Ok(Ok(_)))
    }

    /// Graceful stop ladder (design §7): `init.shutdown` → wait → `SendCtrlAltDel` → wait →
    /// SIGKILL the process group. Returns which step stopped it.
    pub async fn stop(&mut self, cfg: &FirecrackerConfig, api: &mut ApiClient) -> &'static str {
        let t0 = Instant::now();
        if self.wait_exit(Duration::ZERO).await {
            return "already exited";
        }
        let shutdown = async {
            let deadline = Instant::now() + Duration::from_millis(500);
            let (mut ctl, _) = connect_control(&self.vsock(), CONTROL_PORT, deadline).await?;
            ctl.shutdown(cfg.shutdown_grace_ms, Duration::from_millis(500))
                .await
        };
        if let Err(e) = shutdown.await {
            tracing::debug!(sandbox = %self.id, error = %e, "init.shutdown failed");
        }
        if self
            .wait_exit(cfg.ctrl_alt_del_after.saturating_sub(t0.elapsed()))
            .await
        {
            return "init.shutdown";
        }
        let api_timeout = Duration::from_millis(300);
        let cad = tokio::time::timeout(api_timeout, api.action(ActionType::SendCtrlAltDel)).await;
        tracing::debug!(sandbox = %self.id, ?cad, "sent SendCtrlAltDel");
        if self
            .wait_exit(cfg.kill_after.saturating_sub(t0.elapsed()))
            .await
        {
            return "SendCtrlAltDel";
        }
        self.kill_and_wait().await;
        "SIGKILL"
    }

    /// SIGKILL the VMM group and reap it.
    pub async fn kill_and_wait(&mut self) {
        self.kill();
        let _ = self.wait_exit(Duration::from_secs(5)).await;
    }

    fn kill(&mut self) {
        if let Some(vmm) = self.vmm {
            vmm.kill(true);
        }
        if let Some(child) = self.child.as_mut() {
            let _ = child.start_kill();
        }
    }

    /// Final cleanup after the VMM is gone: cgroup leaf, tap, run dir. Rw images listed in
    /// `preserve` are moved to the recovery dir first.
    pub fn cleanup(&mut self, preserve: &[RwDriveMeta]) -> Vec<PathBuf> {
        let moved = rundir::preserve_rw_images(&self.recovery_dir, self.id, preserve);
        if let Some(cg) = self.cgroup.take() {
            cg.remove();
        }
        self.tap = None;
        if let Err(e) = fs::remove_dir_all(&self.dir)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(dir = %self.dir.display(), error = %e, "cannot remove run dir");
        }
        self.done = true;
        moved
    }
}

impl Drop for Vm {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        self.kill();
        if let Some(child) = self.child.as_mut() {
            // Reap synchronously if it is already gone; kill_on_drop + tokio's orphan
            // reaper handle the rest.
            let _ = child.try_wait();
        }
        // Guest writes can only exist once the guest was configured.
        let drives = if self.meta.guest_boot_ms.is_some() {
            self.meta.rw_drives.clone()
        } else {
            Vec::new()
        };
        self.cleanup(&drives);
    }
}

/// Configure and boot a launched VM (all pre-boot PUTs, then `InstanceStart`).
pub(crate) struct BootPlan {
    pub machine: MachineConfig,
    pub boot: BootSource,
    pub drives: Vec<Drive>,
    pub net: Option<NetworkInterface>,
}

pub(crate) async fn configure_and_start(
    vm: &mut Vm,
    api: &mut ApiClient,
    plan: &BootPlan,
    timings: &LifecycleTimings,
) -> Result<()> {
    let ctx = |what: &str, e: crate::api::ApiError| fc_err(what, vm_log(&vm.dir, e.to_string()));
    api.put_machine_config(&plan.machine)
        .await
        .map_err(|e| ctx("machine-config", e))?;
    api.put_boot_source(&plan.boot)
        .await
        .map_err(|e| ctx("boot-source", e))?;
    for d in &plan.drives {
        api.put_drive(d)
            .await
            .map_err(|e| ctx(&format!("drive `{}`", d.drive_id), e))?;
    }
    if let Some(n) = &plan.net {
        api.put_network_interface(n)
            .await
            .map_err(|e| ctx("network interface", e))?;
    }
    api.put_vsock(&Vsock {
        guest_cid: GUEST_CID,
        uds_path: vm.vsock(),
    })
    .await
    .map_err(|e| ctx("vsock", e))?;
    timings.mark_custom("configured");
    api.action(ActionType::InstanceStart)
        .await
        .map_err(|e| ctx("InstanceStart", e))?;
    let started = timings.mark_custom("instance_start");
    timings.mark_at(sandcrab_core::LifecyclePhase::BackendStarted, started);
    Ok(())
}

fn vm_log(dir: &Path, msg: String) -> String {
    let tail = rundir::log_tail(dir, 1024);
    if tail.is_empty() {
        msg
    } else {
        format!("{msg}; firecracker.log: {tail}")
    }
}

/// Connect the control port (retrying until `deadline`) and send `init.configure`.
pub(crate) async fn init_guest(
    vm: &mut Vm,
    init: &InitConfig,
    deadline: Instant,
    timings: &LifecycleTimings,
) -> Result<InitConfigureResult> {
    let vsock = vm.vsock();
    let connect = connect_control(&vsock, CONTROL_PORT, deadline);
    let (mut ctl, attempts) = tokio::select! {
        r = connect => r.map_err(|e| SandboxError::Backend {
            backend: BACKEND.into(),
            message: vm_log(&vm.dir, format!("guest did not come up: {e}")),
        })?,
        status = async {
            match vm.child.as_mut() { Some(c) => c.wait().await.ok(), None => std::future::pending().await }
        } => {
            return Err(fc_err(
                "firecracker exited during boot",
                vm_log(&vm.dir, status.map_or_else(|| "unknown status".into(), |s| s.to_string())),
            ));
        }
    };
    timings.mark_custom("control_connected");
    tracing::debug!(sandbox = %vm.id, attempts, "guest control port connected");
    let remaining = deadline
        .saturating_duration_since(Instant::now())
        .max(Duration::from_secs(1));
    let res = ctl.configure(init, remaining).await?;
    timings.mark_custom("init_configured");
    Ok(res)
}

#[must_use]
pub(crate) fn unix_ms() -> Option<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| u64::try_from(d.as_millis()).ok())
}

pub(crate) fn drive(id: &str, path: &Path, root: bool, read_only: bool) -> Drive {
    Drive {
        drive_id: id.to_owned(),
        path_on_host: path.to_path_buf(),
        is_root_device: root,
        is_read_only: read_only,
        io_engine: Some(IoEngine::Sync),
    }
}

/// A rw workspace that is synced back at teardown.
#[derive(Debug, Clone)]
pub(crate) struct SyncDrive {
    pub meta: RwDriveMeta,
    pub tag: String,
}

/// Unpack caps for one drive: the configured caps bounded by the drive image's size (the
/// most the guest can legitimately hold) and the optional host quota. Also returns that
/// capacity.
pub(crate) fn sync_back_limits(
    cfg: &FirecrackerConfig,
    image: &Path,
) -> Result<(sandcrab_core::archive::UnpackLimits, u64)> {
    let size = fs::metadata(image)
        .map_err(|e| workspace::io_at(image, e))?
        .len();
    let capacity = cfg.sync_back_quota.map_or(size, |q| q.min(size));
    Ok((cfg.unpack_limits.bounded_by(capacity), capacity))
}

/// Live Firecracker sandbox. Dropping it without `destroy` closes the published ports (field
/// order: before the VM is killed).
pub(crate) struct FirecrackerSandbox {
    pub ports: PortForwards,
    pub vm: Vm,
    pub api: ApiClient,
    pub connector: Arc<VsockConnector>,
    pub sync: Vec<SyncDrive>,
    pub timings: LifecycleTimings,
    pub config: Arc<FirecrackerConfig>,
}

impl std::fmt::Debug for FirecrackerSandbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FirecrackerSandbox")
            .field("vm", &self.vm)
            .finish_non_exhaustive()
    }
}

impl FirecrackerSandbox {
    /// Export drive `d` over the control port, unpack into staging, commit to the host.
    async fn sync_back(&self, d: &SyncDrive) -> Result<sandcrab_core::archive::UnpackStats> {
        let cfg = &self.config;
        let m = &d.meta;
        let staging = workspace::staging_dir(&m.backing, &m.host_path, &self.vm.dir, &d.tag);
        workspace::fresh_staging(&staging)?;
        let res = async {
            let (limits, capacity) = sync_back_limits(cfg, &m.image)?;
            let budget = workspace::export_deadline(
                cfg.export_timeout,
                capacity,
                cfg.export_min_bytes_per_sec,
            );
            let deadline = Instant::now() + budget;
            let (ctl, _) = connect_control(
                &self.vm.vsock(),
                CONTROL_PORT,
                Instant::now() + Duration::from_secs(5),
            )
            .await?;
            // The guest pre-scans the tree before acking: allow up to 60 s of the budget.
            let ack_timeout = deadline
                .saturating_duration_since(Instant::now())
                .min(Duration::from_secs(60))
                .max(Duration::from_secs(1));
            let (_ack, stream) = ctl.export(&m.drive_id, ack_timeout).await?;
            let std = stream
                .into_std()
                .and_then(|s| s.set_nonblocking(false).map(|()| s))
                .map_err(|e| SandboxError::Transport(format!("export stream: {e}")))?;
            // Idle timeout + overall deadline: a slow-dripping guest cannot stall destroy.
            let reader = workspace::DeadlineReader::new(std, cfg.export_idle_timeout, deadline);
            let (backing, host, stg) = (m.backing.clone(), m.host_path.clone(), staging.clone());
            let (name, identity) = (m.workspace_name.clone(), m.root_identity);
            tokio::task::spawn_blocking(move || {
                let stats = workspace::receive_export(
                    std::io::BufReader::with_capacity(256 * 1024, reader),
                    &stg,
                    &limits,
                )?;
                // Re-check the root right before writing into it: a root replaced by a
                // symlink (or another directory) since the lease is never synced into.
                sandcrab_core::verify_workspace_root(&name, &host, identity)?;
                workspace::commit(&backing, &stg, &host)?;
                Ok::<_, SandboxError>(stats)
            })
            .await
            .map_err(|e| fc_err("sync-back task", e))?
        }
        .await;
        if res.is_err() {
            let _ = fs::remove_dir_all(&staging);
        }
        res
    }
}

#[async_trait]
impl SandboxHandle for FirecrackerSandbox {
    fn id(&self) -> SandboxId {
        self.vm.id
    }

    fn connector(&self) -> Arc<dyn Connector> {
        self.connector.clone()
    }

    fn published_ports(&self) -> Vec<PublishedPort> {
        self.ports.published().to_vec()
    }

    fn timings(&self) -> LifecycleTimings {
        self.timings.clone()
    }

    async fn destroy(mut self: Box<Self>) -> Result<()> {
        let t = self.timings.clone();
        // 0. Stop accepting: no new tunnels into a VM that is going away.
        std::mem::replace(&mut self.ports, PortForwards::none())
            .shutdown()
            .await;
        // 1. Sync rw workspaces back while the guest is still up.
        let mut failed: Vec<(RwDriveMeta, String)> = Vec::new();
        for d in self.sync.clone() {
            match self.sync_back(&d).await {
                Ok(stats) => tracing::debug!(
                    sandbox = %self.vm.id, workspace = %d.meta.workspace_name, ?stats,
                    "workspace synced back"
                ),
                Err(e) => failed.push((d.meta.clone(), e.to_string())),
            }
        }
        t.mark_custom("ws_sync_done");
        // 2. Stop the VMM.
        let cfg = Arc::clone(&self.config);
        let this = &mut *self;
        let how = this.vm.stop(&cfg, &mut this.api).await;
        t.mark_custom("vmm_exited");
        tracing::debug!(sandbox = %self.vm.id, how, "firecracker stopped");
        // 3. Release everything; keep images whose sync-back failed.
        let preserve: Vec<_> = failed.iter().map(|(m, _)| m.clone()).collect();
        let moved = self.vm.cleanup(&preserve);
        t.mark_custom("cleanup_done");
        if failed.is_empty() {
            return Ok(());
        }
        let detail = failed
            .iter()
            .map(|(m, e)| format!("`{}` ({e})", m.workspace_name))
            .collect::<Vec<_>>()
            .join(", ");
        Err(SandboxError::Workspace(format!(
            "sync-back failed for {detail}; the guest's writes were not (fully) committed and \
             the guest image(s) were preserved at {moved:?}"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sync_back_limits_follow_drive_size_and_quota() {
        let t = tempfile::tempdir().unwrap();
        let img = t.path().join("ws.ext4");
        fs::File::create(&img)
            .unwrap()
            .set_len(2 << 30) // sparse 2 GiB image
            .unwrap();
        let mut cfg = FirecrackerConfig::with_home(t.path(), "/c");
        let (l, cap) = sync_back_limits(&cfg, &img).unwrap();
        assert_eq!(cap, 2 << 30);
        let slack = sandcrab_core::archive::CAPACITY_SLACK_BYTES;
        assert_eq!(l.max_total_bytes, (2 << 30) + slack);
        assert_eq!(l.max_file_bytes, l.max_total_bytes);
        cfg.sync_back_quota = Some(100 << 20);
        let (l, _) = sync_back_limits(&cfg, &img).unwrap();
        assert_eq!(l.max_total_bytes, (100 << 20) + slack);
        assert!(sync_back_limits(&cfg, &t.path().join("missing")).is_err());
    }
}
