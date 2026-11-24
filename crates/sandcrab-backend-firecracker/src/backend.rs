//! [`FirecrackerBackend`]: the hardware-isolated `RuntimeBackend` (design doc
//! `docs/design/firecracker-backend.md`).

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use async_trait::async_trait;
use sandcrab_core::{
    BackendCapabilities, BackendKind, Connector, ForwarderConfig, MountMode, NetworkConfig,
    NetworkPolicy, PortForwards, ResolvedSpec, Result, RuntimeBackend, SandboxError, SandboxHandle,
    StopSignal, UserSpec, WorkspaceBacking,
};
use sandcrab_protocol::init::{
    DEFAULT_TOOLS_PORT, GuestMount, InitConfig, InitUser, ROOT_DRIVE_ID, SCRATCH_DRIVE_ID,
};

use crate::api::{ApiClient, BootSource, MachineConfig, NetworkInterface};
use crate::config::{FirecrackerConfig, RootfsSource};
use crate::net::acquire_tap;
use crate::proc::ProcId;
use crate::rootfs::{Arch, MkfsStrategy, RootfsBuilder, RootfsRequest};
use crate::rundir::{self, CgroupLeaf, RwDriveMeta, SCRATCH_FILE};
use crate::vm::{
    BACKEND, BootPlan, FirecrackerSandbox, SyncDrive, Vm, configure_and_start, drive, fc_err,
    init_guest, unix_ms,
};
use crate::vsock::VsockConnector;
use crate::workspace;

/// Default KVM device.
pub const KVM_DEVICE: &str = "/dev/kvm";

/// Check that `device` (normally `/dev/kvm`) can be opened read-write.
pub fn check_kvm(device: &Path) -> Result<()> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(device)
        .map(drop)
        .map_err(|e| {
            let hint = match e.kind() {
                std::io::ErrorKind::NotFound => {
                    "this host has no KVM (bare metal with VT-x/AMD-V, or a VM with nested \
                     virtualization enabled, is required)"
                }
                std::io::ErrorKind::PermissionDenied => {
                    "add the user to the `kvm` group (`sudo usermod -aG kvm $USER`, then log in again)"
                }
                _ => "check the KVM device",
            };
            SandboxError::Backend {
                backend: BACKEND.into(),
                message: format!("{} is not usable: {e}; {hint}", device.display()),
            }
        })
}

/// Hardware-isolated Firecracker microVM backend (V1: no jailer, so not multi-tenant safe).
///
/// Every sandbox is one unprivileged `firecracker` process with its built-in seccomp
/// filters, booting the pinned 6.18 guest kernel with `crabd` as PID 1. Host↔guest traffic
/// is the regular Sandcrab protocol over vsock. Workspaces are copied in as ext4 drives and
/// synced back at teardown (no live sharing: `supports_host_dir_mounts = false`).
#[derive(Debug, Clone)]
pub struct FirecrackerBackend {
    config: Arc<FirecrackerConfig>,
    mkfs: MkfsStrategy,
    host_user: UserSpec,
    /// (image, pin) → verified rootfs path, per process.
    rootfs_cache: RootfsCache,
}

/// (image, pin) → verified rootfs image.
type RootfsCache = Arc<Mutex<HashMap<(String, Option<String>), PathBuf>>>;

impl FirecrackerBackend {
    /// Validate the host and config: `/dev/kvm` read-write, the `firecracker` binary and
    /// kernel present (and matching their SHA-256 pins when `verify_pins`), a usable
    /// `mke2fs`. Then reap orphans of dead sandcrab processes.
    pub fn new(config: FirecrackerConfig) -> Result<Self> {
        check_kvm(Path::new(KVM_DEVICE))?;
        check_executable(&config.firecracker_bin)?;
        if !config.kernel.is_file() {
            return Err(SandboxError::InvalidConfig(format!(
                "guest kernel `{}` not found; fetch it with `scripts/fetch-artifacts.sh` (or \
                 FirecrackerConfig::fetch_pinned_artifacts)",
                config.kernel.display()
            )));
        }
        if config.verify_pins {
            use crate::artifacts::{FIRECRACKER_X86_64, KERNEL_X86_64};
            let verify = |p: &Path, sha: &str| {
                crate::hash::verify_sha256(p, sha).map_err(|e| SandboxError::DigestMismatch {
                    image: p.display().to_string(),
                    expected: sha.to_owned(),
                    actual: e.to_string(),
                })
            };
            verify(&config.kernel, KERNEL_X86_64.sha256)?;
            verify(
                &config.firecracker_bin,
                FIRECRACKER_X86_64.firecracker_sha256,
            )?;
        }
        let backend = Self::new_unchecked(config)?;
        match backend.reap_orphans() {
            Ok(r) if !r.is_empty() => tracing::info!(?r, "reaped orphaned firecracker sandboxes"),
            Ok(_) => {}
            Err(e) => tracing::warn!(error = %e, "orphan reaping failed"),
        }
        Ok(backend)
    }

    /// Construct without the host checks of [`Self::new`] (no `/dev/kvm`, binary, kernel or
    /// pin checks, no orphan reaping). For tests with a fake `firecracker`, and for tools
    /// that only need the configuration.
    pub fn new_unchecked(config: FirecrackerConfig) -> Result<Self> {
        config.validate()?;
        let mkfs = match config.mkfs {
            Some(s) => s,
            None => {
                crate::workspace::create_private_dir_all(&config.cache_root)?;
                crate::rootfs::mkfs::detect(&config.cache_root)
                    .map_err(|e| fc_err("no usable mke2fs for workspace images", e))?
            }
        };
        Ok(Self {
            config: Arc::new(config),
            mkfs,
            host_user: UserSpec::new(
                rustix::process::getuid().as_raw(),
                rustix::process::getgid().as_raw(),
            ),
            rootfs_cache: Arc::default(),
        })
    }

    #[must_use]
    pub fn config(&self) -> &FirecrackerConfig {
        &self.config
    }

    /// Guest user: spec → configured default → host uid:gid.
    fn user(&self, spec: &ResolvedSpec) -> UserSpec {
        spec.user
            .or(self.config.default_user)
            .unwrap_or(self.host_user)
    }

    /// Rootfs image for the spec's template (cached per process; built on a cache miss).
    async fn rootfs(&self, spec: &ResolvedSpec) -> Result<PathBuf> {
        let crabd = match &self.config.rootfs {
            RootfsSource::Image(p) => return Ok(p.clone()),
            RootfsSource::Template { crabd } => crabd.clone(),
        };
        let key = (spec.image.clone(), spec.image_sha256.clone());
        if let Some(p) = self
            .rootfs_cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&key)
        {
            return Ok(p.clone());
        }
        // Never build a rootfs from an image this host has not pinned (local integrity).
        if spec.image_sha256.is_none() {
            return Err(sandcrab_core::pins::unpinned_error(
                &spec.template.manifest,
                sandcrab_core::ImageArch::host(),
                None,
            ));
        }
        let req = RootfsRequest {
            image: spec.image.clone(),
            expected_image_id: spec.image_sha256.clone(),
            template: Some(spec.template.name().to_owned()),
            crabd,
            // Firecracker runs guests of the host arch only.
            arch: Arch::host().map_err(|e| fc_err("rootfs", e))?,
        };
        let builder = RootfsBuilder::new(&self.config.cache_root);
        let built = tokio::task::spawn_blocking(move || builder.build(&req))
            .await
            .map_err(|e| fc_err("rootfs build task", e))?
            .map_err(|e| match e {
                crate::Error::ImagePinMismatch {
                    image,
                    expected,
                    actual,
                } => SandboxError::DigestMismatch {
                    image,
                    expected,
                    actual: format!(
                        "{actual} ({})",
                        sandcrab_core::pins::mismatch_hint(spec.template.name())
                    ),
                },
                other => fc_err("rootfs", other),
            })?;
        tracing::debug!(
            image = %spec.image, cache_hit = built.cache_hit, elapsed = ?built.elapsed,
            "rootfs ready"
        );
        self.rootfs_cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(key, built.image_path.clone());
        Ok(built.image_path)
    }

    async fn spawn_vm(
        &self,
        spec: &ResolvedSpec,
        vm: &mut Vm,
    ) -> Result<(ApiClient, Vec<SyncDrive>)> {
        let cfg = &self.config;
        let t = &spec.timings;
        let user = self.user(spec);
        let owner = (user.uid, user.gid);

        // Root + scratch drives.
        let rootfs = self.rootfs(spec).await?;
        let scratch = vm.dir.join(SCRATCH_FILE);
        {
            let (cache, mib, dest) = (
                cfg.cache_root.clone(),
                spec.resources.disk_mb,
                scratch.clone(),
            );
            tokio::task::spawn_blocking(move || workspace::scratch_drive(&cache, mib, &dest))
                .await
                .map_err(|e| fc_err("scratch task", e))??;
        }
        t.mark_custom("rootfs_ready");

        // Workspace drives.
        t.mark_custom("ws_attach_start");
        let mut drives = vec![
            drive(ROOT_DRIVE_ID, &rootfs, true, true),
            drive(SCRATCH_DRIVE_ID, &scratch, false, false),
        ];
        let mut guest_mounts = Vec::new();
        let mut sync = Vec::new();
        for (n, m) in spec.mounts.iter().enumerate() {
            let drive_id = format!("ws{n}");
            let read_only = m.mode == MountMode::ReadOnly;
            // Re-check the leased root (symlink-free, same dev/ino) right before packing it.
            if m.backing.is_dir() {
                sandcrab_core::verify_workspace_root(
                    &m.workspace_name,
                    &m.host_path,
                    m.root_identity,
                )?;
            }
            let image = match &m.backing {
                WorkspaceBacking::BlockVolume(_) => m.host_path.clone(),
                _ if read_only => {
                    let (strategy, cache, wid, generation, root, label) = (
                        self.mkfs,
                        cfg.cache_root.join("ws-ro"),
                        m.workspace_id.to_string(),
                        m.generation,
                        m.host_path.clone(),
                        drive_id.clone(),
                    );
                    let (img, hit) = tokio::task::spawn_blocking(move || {
                        workspace::cached_ro_image(
                            strategy, &cache, &wid, generation, &root, owner, &label,
                        )
                    })
                    .await
                    .map_err(|e| fc_err("workspace image task", e))??;
                    tracing::debug!(workspace = %m.workspace_name, cache_hit = hit, "ro image");
                    img
                }
                _ => {
                    let img = vm.dir.join(format!("ws-{n}.ext4"));
                    let (strategy, root, dir, img2, label) = (
                        self.mkfs,
                        m.host_path.clone(),
                        vm.dir.clone(),
                        img.clone(),
                        drive_id.clone(),
                    );
                    tokio::task::spawn_blocking(move || {
                        let opts = workspace::ImageOpts {
                            label: &label,
                            owner,
                            read_only: false,
                            quota_mb: None,
                        };
                        workspace::build_image(strategy, &root, &dir, &img2, &opts)
                    })
                    .await
                    .map_err(|e| fc_err("workspace image task", e))??;
                    let meta = RwDriveMeta {
                        drive_id: drive_id.clone(),
                        image: img.clone(),
                        workspace_id: m.workspace_id.to_string(),
                        workspace_name: m.workspace_name.clone(),
                        backing: m.backing.clone(),
                        host_path: m.host_path.clone(),
                        guest_path: m.guest_path.clone(),
                        root_identity: m.root_identity,
                    };
                    vm.meta.rw_drives.push(meta.clone());
                    sync.push(SyncDrive {
                        meta,
                        tag: format!("{}-{n}", spec.id),
                    });
                    img
                }
            };
            drives.push(drive(&drive_id, &image, false, read_only));
            guest_mounts.push(GuestMount::block(
                drive_id.clone(),
                Some(drive_id),
                m.guest_path.clone(),
                read_only,
            ));
        }
        vm.meta.save(&vm.dir)?;
        t.mark_custom("ws_attach_done");

        // Network.
        let net = if spec.network.policy == NetworkPolicy::Unrestricted {
            let lease = acquire_tap(&cfg.taps, self.host_user.uid)?;
            vm.meta.tap = Some(lease.plan.name.clone());
            vm.tap = Some(lease);
            vm.meta.save(&vm.dir)?;
            vm.tap.as_ref().map(|l| l.plan.clone())
        } else {
            None
        };

        // VMM.
        vm.launch(cfg)?;
        if let Some(parent) = &cfg.cgroup_parent {
            let mem = (spec.resources.memory_mb + cfg.vmm_overhead_mib) * 1024 * 1024;
            let leaf = CgroupLeaf::create(parent, spec.id, mem, cfg.pids_max)?;
            vm.meta.cgroup = Some(leaf.path.clone());
            let pid = vm.vmm.map(|p| p.pid).unwrap_or_default();
            vm.cgroup = Some(leaf);
            vm.cgroup.as_ref().expect("set").add(pid)?;
            vm.meta.save(&vm.dir)?;
        }
        let deadline = Instant::now() + cfg.boot_timeout;
        let mut api = ApiClient::new(vm.api_sock()).with_timeout(cfg.api_timeout);
        vm.wait_api(&mut api, deadline).await?;
        t.mark_custom("vmm_started");

        let plan = BootPlan {
            machine: MachineConfig {
                vcpu_count: spec.resources.vcpu,
                mem_size_mib: spec.resources.memory_mb,
                smt: false,
            },
            boot: BootSource {
                kernel_image_path: cfg.kernel.clone(),
                boot_args: cfg.boot_args(net.as_ref().map(|p| p.kernel_ip_arg()).as_deref()),
            },
            drives,
            net: net.as_ref().map(|p| NetworkInterface {
                iface_id: "eth0".into(),
                host_dev_name: p.name.clone(),
                guest_mac: Some(p.guest_mac.clone()),
            }),
        };
        configure_and_start(vm, &mut api, &plan, t).await?;

        let init = InitConfig {
            sandbox_id: spec.id.to_string(),
            hostname: format!("sc-{}", &spec.id.to_string()[..8]),
            user: InitUser {
                uid: user.uid,
                gid: user.gid,
                home: None,
                name: None,
            },
            env: spec.env.clone(),
            default_cwd: spec.default_cwd.clone(),
            mounts: guest_mounts,
            network: net.is_some(),
            dns: if net.is_some() {
                cfg.dns.clone()
            } else {
                Vec::new()
            },
            tools_port: DEFAULT_TOOLS_PORT,
            host_time_unix_ms: unix_ms(),
        };
        let configured = init_guest(vm, &init, deadline, t).await?;
        vm.meta.guest_boot_ms = Some(configured.boot_ms);
        vm.meta.save(&vm.dir)?;
        tracing::debug!(
            sandbox = %spec.id, guest_boot_ms = configured.boot_ms, serve_pid = configured.serve_pid,
            "guest configured"
        );

        Ok((api, sync))
    }
}

fn check_executable(p: &Path) -> Result<()> {
    let ok = std::fs::metadata(p)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false);
    if ok {
        Ok(())
    } else {
        Err(SandboxError::InvalidConfig(format!(
            "firecracker binary `{}` is missing or not executable; fetch it with \
             `scripts/fetch-artifacts.sh` (or FirecrackerConfig::fetch_pinned_artifacts)",
            p.display()
        )))
    }
}

/// Network policies this backend enforces (no silent downgrade): `disabled` = no NIC,
/// `unrestricted` = tap + NAT. Domain allow-lists cannot be enforced in V1.
/// Resolves once the VMM process is gone (crash watch for the published ports; the VMM is
/// owned by `Vm`, so its liveness is polled by identity).
fn vmm_gone(vmm: ProcId) -> StopSignal {
    Box::pin(async move {
        while vmm.is_alive() {
            tokio::time::sleep(VMM_POLL).await;
        }
    })
}

/// How often the published-port crash watch checks the VMM.
const VMM_POLL: std::time::Duration = std::time::Duration::from_millis(250);

fn enforces(network: &NetworkConfig) -> bool {
    matches!(
        network.policy,
        NetworkPolicy::Disabled | NetworkPolicy::Unrestricted
    )
}

#[async_trait]
impl RuntimeBackend for FirecrackerBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Firecracker
    }

    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities {
            hardware_isolated: true,
            // No jailer / per-VM netns in V1 (design §8).
            multi_tenant_safe: false,
            supports_snapshots: false,
            // Workspaces are block drives copied in and synced back, not live shares.
            supports_host_dir_mounts: false,
        }
    }

    fn enforces_network_policy(&self, network: &NetworkConfig) -> bool {
        enforces(network)
    }

    async fn spawn(&self, spec: &ResolvedSpec) -> Result<Box<dyn SandboxHandle>> {
        spec.reject_secrets()?;
        if !enforces(&spec.network) {
            return Err(SandboxError::PolicyUnenforceable(format!(
                "the firecracker backend cannot enforce network policy `{}` in V1 \
                 (use `disabled` or `unrestricted`)",
                spec.network.policy
            )));
        }
        sandcrab_core::validate_ports(&spec.ports)?;
        let (dir, meta) = rundir::create(&self.config.run_dir, spec.id)?;
        let mut vm = Vm::new(spec.id, dir, meta, self.config.recovery_dir.clone());
        let started = async {
            let (api, sync) = self.spawn_vm(spec, &mut vm).await?;
            let connector = Arc::new(VsockConnector::new(vm.vsock(), DEFAULT_TOOLS_PORT));
            // Published ports: 127.0.0.1 listeners → `tunnel.open` over vsock (design §6).
            // Firecracker's vsock bridge does not preserve half-close (measured on KVM): a
            // host-side SHUT_WR becomes a full close (the guest's reply would be lost) and a
            // guest-side SHUT_WR never reaches the host. So the client's half-close is not
            // forwarded; request/response still works.
            let ports = PortForwards::bind(
                Arc::clone(&connector) as Arc<dyn Connector>,
                &spec.ports,
                ForwarderConfig {
                    forward_half_close: false,
                    ..ForwarderConfig::default()
                },
                vm.vmm.map(vmm_gone),
            )
            .await?;
            if !ports.is_empty() {
                spec.timings.mark_custom("ports_published");
            }
            Ok::<_, SandboxError>((api, sync, connector, ports))
        }
        .await;
        match started {
            Ok((api, sync, connector, ports)) => Ok(Box::new(FirecrackerSandbox {
                ports,
                connector,
                vm,
                api,
                sync,
                timings: spec.timings.clone(),
                config: Arc::clone(&self.config),
            })),
            Err(e) => {
                // Nothing was synced yet: rw images hold no guest writes, so do not keep them.
                vm.meta.rw_drives.clear();
                if vm.child.is_some() {
                    vm.kill_and_wait().await;
                }
                vm.cleanup(&[]);
                Err(e)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kvm_check_reports_missing_device() {
        let dir = tempfile::tempdir().unwrap();
        let err = check_kvm(&dir.path().join("kvm")).unwrap_err().to_string();
        assert!(err.contains("no KVM"), "{err}");
    }

    #[test]
    fn policy_matrix() {
        assert!(enforces(&NetworkConfig::disabled()));
        assert!(enforces(&NetworkConfig::unrestricted()));
        assert!(!enforces(&NetworkConfig::egress_restricted([
            "example.com"
        ])));
    }
}
