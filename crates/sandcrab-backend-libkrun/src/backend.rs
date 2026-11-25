//! `RuntimeBackend` impl, capabilities and network-policy mapping (design §9, §11).

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use async_trait::async_trait;
use sandcrab_core::{
    BackendCapabilities, BackendKind, LifecyclePhase, NetworkConfig, ResolvedSpec, Result,
    RuntimeBackend, SandboxError, SandboxHandle, UserSpec,
};
use sandcrab_protocol::init::{InitConfig, InitUser};

use crate::config::{LibkrunConfig, RootfsSource};
use crate::helper_proto::{
    DiskSpec, GuestExec, HELPER_PROTO_VERSION, HelperConfig, HelperNetwork, MAX_MEMORY_MIB,
    MIN_MEMORY_MIB, NetstackSpec, PinnedFile, ROOT_DRIVE_ID, SCRATCH_DRIVE_ID, TOOLS_PORT,
};
use crate::net::{self, NetOptions};
use crate::netstack::{self, NetstackKind};
use crate::ports;
use crate::reap::{self, ReapReport};
use crate::vm::{BACKEND, KrunSandbox, KrunVm, Launch, SCRATCH_FILE, krun_err, scratch_disk};
use crate::workspace::{self, MountPlan};

/// Capabilities of the libkrun backend (design §11).
///
/// - `hardware_isolated`: a separate guest kernel behind Hypervisor.framework / KVM.
/// - `multi_tenant_safe: false`: the device model (virtio-fs server, vsock, block) runs
///   in-process as the host user without seccomp/Landlock/Seatbelt, so a device-emulation
///   escape gets that user's full privileges (Phase C confines the helper).
/// - `supports_snapshots: false`: libkrun has no snapshot/restore API.
/// - `supports_host_dir_mounts: true`: live virtio-fs shares.
pub const CAPABILITIES: BackendCapabilities = BackendCapabilities {
    hardware_isolated: true,
    multi_tenant_safe: false,
    supports_snapshots: false,
    supports_host_dir_mounts: true,
};

/// Nameservers written to the guest's `/etc/resolv.conf` under the TSI opt-in (TSI proxies
/// guest sockets through the host, but the guest still needs a resolver address).
pub const TSI_DNS: [IpAddr; 2] = [
    IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1)),
    IpAddr::V4(Ipv4Addr::new(9, 9, 9, 9)),
];

/// The libkrun + libkrunfw files handed to the helper, each pinned by SHA-256, plus the
/// optional pinned userspace netstack. The helper hashes them again before `dlopen` / spawn.
/// Build it from the pinned artifacts (`artifacts.rs`) or, for a `lib_dir` override, with
/// [`KrunLibs::hash_files`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KrunLibs {
    pub libkrun: PinnedFile,
    pub libkrunfw: PinnedFile,
    /// passt (Linux) / gvproxy (macOS) for `network.policy = unrestricted` (design §9 Phase
    /// B). `None` = not installed: `unrestricted` is then `PolicyUnenforceable`.
    pub netstack: Option<NetstackBin>,
}

/// A pinned userspace netstack executable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetstackBin {
    pub kind: NetstackKind,
    pub binary: PinnedFile,
}

impl KrunLibs {
    /// Pin whatever is at these paths now (trust on first use: for a `lib_dir` override and
    /// tests; prefer the release pins).
    pub fn hash_files(libkrun: &Path, libkrunfw: &Path) -> Result<Self> {
        let pin = |p: &Path| -> Result<PinnedFile> {
            Ok(PinnedFile {
                path: p.to_path_buf(),
                sha256: sandcrab_rootfs::hash::sha256_file(p)
                    .map_err(|e| krun_err("hashing libkrun", e))?,
            })
        };
        Ok(Self {
            libkrun: pin(libkrun)?,
            libkrunfw: pin(libkrunfw)?,
            netstack: None,
        })
    }

    /// Directory libkrun finds libkrunfw in (exported to the helper).
    #[must_use]
    pub fn libkrunfw_dir(&self) -> &Path {
        self.libkrunfw.path.parent().unwrap_or(Path::new("/"))
    }

    /// Every file exists; with `verify`, each matches its pin.
    pub fn check(&self, verify: bool) -> Result<()> {
        let netstack = self.netstack.as_ref().map(|n| &n.binary);
        for f in [&self.libkrun, &self.libkrunfw].into_iter().chain(netstack) {
            if !f.path.is_absolute() || !f.path.is_file() {
                return Err(SandboxError::InvalidConfig(format!(
                    "libkrun library `{}` not found (install the pinned artifacts with \
                     `sandcrab libkrun install`)",
                    f.path.display()
                )));
            }
            if verify {
                sandcrab_rootfs::hash::verify_sha256(&f.path, &f.sha256).map_err(|e| {
                    SandboxError::DigestMismatch {
                        image: f.path.display().to_string(),
                        expected: f.sha256.clone(),
                        actual: e.to_string(),
                    }
                })?;
            }
        }
        Ok(())
    }
}

/// Hardware-isolated libkrun microVM backend (Apple Silicon macOS and Linux KVM).
///
/// Every sandbox is one signed `sandcrab-krun` helper process that becomes the VMM, booting
/// libkrunfw's kernel with `crabd init` as PID 1 on a read-only ext4 root plus a scratch
/// overlay. Host↔guest traffic is the regular Sandcrab protocol over vsock ports mapped to
/// Unix sockets in the run dir. Workspaces are live virtio-fs shares. TSI networking is never
/// enabled implicitly.
#[derive(Debug, Clone)]
pub struct LibkrunBackend {
    config: Arc<LibkrunConfig>,
    libs: Arc<KrunLibs>,
    host_user: UserSpec,
    rootfs_cache: RootfsCache,
}

/// (image, pin) → root disk, per process.
type RootfsCache = Arc<Mutex<HashMap<(String, Option<String>), DiskSpec>>>;

impl LibkrunBackend {
    /// Validate the host and config: config invariants, an executable helper (on macOS
    /// signed with `com.apple.security.hypervisor`), the libraries present (and matching
    /// their pins when `verify_pins`), `/dev/kvm` usable on Linux. Then reap orphans of
    /// dead sandcrab processes.
    pub fn new(config: LibkrunConfig, libs: KrunLibs) -> Result<Self> {
        config.validate()?;
        check_executable(&config.helper)?;
        #[cfg(target_os = "macos")]
        check_entitlement(&config.helper)?;
        #[cfg(target_os = "linux")]
        check_kvm(Path::new("/dev/kvm"))?;
        #[cfg(target_os = "linux")]
        check_cgroup(config.cgroup_parent.as_deref())?;
        libs.check(config.verify_pins)?;
        let backend = Self::new_unchecked(config, libs)?;
        match backend.reap_orphans() {
            Ok(r) if !r.is_empty() => tracing::info!(?r, "reaped orphaned libkrun sandboxes"),
            Ok(_) => {}
            Err(e) => tracing::warn!(error = %e, "libkrun orphan reaping failed"),
        }
        Ok(backend)
    }

    /// Construct without host checks or orphan reaping (fake-helper tests, tools that only
    /// need the configuration). The config is still validated.
    pub fn new_unchecked(config: LibkrunConfig, libs: KrunLibs) -> Result<Self> {
        config.validate()?;
        if config.cgroup_parent.is_some() && !cfg!(target_os = "linux") {
            tracing::warn!("libkrun cgroup_parent is ignored on this OS (no cgroups)");
        }
        Ok(Self {
            config: Arc::new(config),
            libs: Arc::new(libs),
            host_user: UserSpec::new(
                rustix::process::getuid().as_raw(),
                rustix::process::getgid().as_raw(),
            ),
            rootfs_cache: Arc::default(),
        })
    }

    #[must_use]
    pub fn config(&self) -> &LibkrunConfig {
        &self.config
    }

    #[must_use]
    pub fn libs(&self) -> &KrunLibs {
        &self.libs
    }

    /// Reap run dirs left behind by dead sandcrab processes (runs in [`Self::new`]).
    pub fn reap_orphans(&self) -> Result<ReapReport> {
        reap::reap_run_dir(&self.config.run_dir)
    }

    fn net_options(&self) -> NetOptions {
        NetOptions {
            allow_tsi_unrestricted: self.config.allow_tsi_unrestricted,
            userspace: self.libs.netstack.as_ref().map(|n| n.kind),
        }
    }

    /// Guest user: spec → configured default → host uid:gid (matches virtio-fs ownership,
    /// the same default as Docker).
    fn user(&self, spec: &ResolvedSpec) -> UserSpec {
        effective_user(spec.user, self.config.default_user, self.host_user)
    }

    /// Read-only root disk for the spec's template (cached per process; built on a miss).
    async fn root_disk(&self, spec: &ResolvedSpec) -> Result<DiskSpec> {
        let crabd = match &self.config.rootfs {
            RootfsSource::Image(p) => return Ok(root_disk(p.clone())),
            RootfsSource::Template { crabd } => crabd.clone(),
        };
        let key = (spec.image.clone(), spec.image_sha256.clone());
        if let Some(d) = self
            .rootfs_cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&key)
        {
            return Ok(d.clone());
        }
        // Never build a rootfs from an image this host has not pinned (local integrity).
        if spec.image_sha256.is_none() {
            return Err(sandcrab_core::pins::unpinned_error(
                &spec.template.manifest,
                sandcrab_core::ImageArch::host(),
                None,
            ));
        }
        let req = sandcrab_rootfs::RootfsRequest {
            image: spec.image.clone(),
            expected_image_id: spec.image_sha256.clone(),
            template: Some(spec.template.name().to_owned()),
            crabd,
            // HVF and KVM run guests of the host arch only.
            arch: sandcrab_rootfs::Arch::host().map_err(|e| krun_err("rootfs", e))?,
        };
        let builder = sandcrab_rootfs::RootfsBuilder::new(&self.config.cache_root);
        let built = tokio::task::spawn_blocking(move || builder.build(&req))
            .await
            .map_err(|e| krun_err("rootfs build task", e))?
            .map_err(|e| match e {
                sandcrab_rootfs::Error::ImagePinMismatch {
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
                other => krun_err("rootfs", other),
            })?;
        tracing::debug!(
            image = %spec.image, cache_hit = built.cache_hit, elapsed = ?built.elapsed,
            "rootfs ready"
        );
        let disk = root_disk(built.image_path);
        self.rootfs_cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(key, disk.clone());
        Ok(disk)
    }

    async fn spawn_vm(&self, spec: &ResolvedSpec, vm: &mut KrunVm) -> Result<()> {
        let cfg = &self.config;
        let t = &spec.timings;
        let network = net::plan(&spec.network, self.net_options())?;
        let (vcpus, memory_mib) = vm_size(spec.resources.vcpu, spec.resources.memory_mb)?;
        let user = self.user(spec);

        // Disks.
        let root = self.root_disk(spec).await?;
        let scratch = vm.dir.join(SCRATCH_FILE);
        {
            let (cache, mib, dest) = (
                cfg.cache_root.clone(),
                spec.resources.disk_mb,
                scratch.clone(),
            );
            tokio::task::spawn_blocking(move || scratch_disk(&cache, mib, &dest))
                .await
                .map_err(|e| krun_err("scratch task", e))??;
        }
        t.mark_custom("rootfs_ready");

        // Workspaces: re-verify each leased root, then plan the shares.
        let plan = workspace::plan_mounts(&spec.mounts)?;
        t.mark_custom("ws_attach_done");

        let netstack = match (network, &self.libs.netstack) {
            (HelperNetwork::Userspace, Some(ns)) => Some(NetstackSpec {
                kind: ns.kind,
                binary: ns.binary.clone(),
                // The host's own addresses and on-link subnets, now (design §9).
                deny: netstack::host_prefixes()
                    .map_err(|e| krun_err("listing host addresses (getifaddrs)", e))?,
            }),
            _ => None,
        };

        let helper_cfg = build_helper_config(&HelperInputs {
            libs: &self.libs,
            vcpus,
            memory_mib,
            root,
            scratch,
            plan: &plan,
            config: cfg,
            control_socket: vm.control_socket(),
            tools_socket: vm.tools_socket(),
            network,
            netstack,
        });
        let line = helper_cfg
            .to_line()
            .map_err(|e| SandboxError::InvalidConfig(format!("libkrun helper config: {e}")))?;

        let deadline = Instant::now() + cfg.boot_timeout;
        let cgroup_mem = (spec.resources.memory_mb + cfg.vmm_overhead_mib) * 1024 * 1024;
        vm.launch(&Launch {
            helper: &cfg.helper,
            lib_dir: self.libs.libkrunfw_dir(),
            config_line: &line,
            cgroup: cfg
                .cgroup_parent
                .as_deref()
                .map(|p| (p, cgroup_mem, cfg.pids_max)),
        })
        .await?;
        let started = t.mark_custom("helper_started");
        t.mark_at(LifecyclePhase::BackendStarted, started);

        let init = init_config(spec, user, &plan, network);
        let configured = vm.configure(&init, deadline, t).await?;
        vm.meta.guest_boot_ms = Some(configured.boot_ms);
        vm.meta.save(&vm.dir)?;
        tracing::debug!(
            sandbox = %spec.id, guest_boot_ms = configured.boot_ms,
            serve_pid = configured.serve_pid, "guest configured"
        );
        vm.wait_tools(deadline, t).await
    }
}

fn root_disk(path: PathBuf) -> DiskSpec {
    // Integrity of a built rootfs is checked by the rootfs cache (manifest + verify on hit);
    // re-hashing a multi-hundred-MiB image in the helper on every spawn would dominate boot.
    DiskSpec {
        block_id: ROOT_DRIVE_ID.into(),
        path,
        read_only: true,
        sha256: None,
    }
}

/// Guest user: spec → configured default → host uid:gid.
fn effective_user(spec: Option<UserSpec>, default: Option<UserSpec>, host: UserSpec) -> UserSpec {
    spec.or(default).unwrap_or(host)
}

/// `krun_set_vm_config` takes a `u8` vCPU count and a `u32` MiB size.
fn vm_size(vcpu: u32, memory_mb: u64) -> Result<(u8, u32)> {
    let vcpus = u8::try_from(vcpu).ok().filter(|v| *v > 0).ok_or_else(|| {
        SandboxError::InvalidConfig(format!("libkrun supports 1..=255 vCPUs, got {vcpu}"))
    })?;
    let memory_mib = u32::try_from(memory_mb)
        .ok()
        .filter(|m| (MIN_MEMORY_MIB..=MAX_MEMORY_MIB).contains(m))
        .ok_or_else(|| {
            SandboxError::InvalidConfig(format!(
                "libkrun guest memory must be {MIN_MEMORY_MIB}..={MAX_MEMORY_MIB} MiB, got {memory_mb}"
            ))
        })?;
    Ok((vcpus, memory_mib))
}

pub(crate) struct HelperInputs<'a> {
    pub libs: &'a KrunLibs,
    pub vcpus: u8,
    pub memory_mib: u32,
    pub root: DiskSpec,
    pub scratch: PathBuf,
    pub plan: &'a MountPlan,
    pub config: &'a LibkrunConfig,
    pub control_socket: PathBuf,
    pub tools_socket: PathBuf,
    pub network: HelperNetwork,
    pub netstack: Option<NetstackSpec>,
}

/// The helper's whole VM description. Validated by `HelperConfig::to_line` before it is sent.
pub(crate) fn build_helper_config(i: &HelperInputs<'_>) -> HelperConfig {
    HelperConfig {
        version: HELPER_PROTO_VERSION,
        libkrun: i.libs.libkrun.clone(),
        libkrunfw: i.libs.libkrunfw.clone(),
        vcpus: i.vcpus,
        memory_mib: i.memory_mib,
        root: i.root.clone(),
        scratch: DiskSpec {
            block_id: SCRATCH_DRIVE_ID.into(),
            path: i.scratch.clone(),
            read_only: false,
            sha256: None,
        },
        disks: i.plan.disks.clone(),
        virtiofs: i.plan.shares.clone(),
        virtiofs_semantics: i.config.virtiofs_semantics,
        control_socket: i.control_socket.clone(),
        tools_socket: i.tools_socket.clone(),
        network: i.network,
        netstack: i.netstack.clone(),
        // Fixed `crabd init` argv/env: everything else goes over `init.configure` (design §6).
        exec: GuestExec::default(),
        log_level: i.config.log_level,
    }
}

/// `init.configure` for the guest.
pub(crate) fn init_config(
    spec: &ResolvedSpec,
    user: UserSpec,
    plan: &MountPlan,
    network: HelperNetwork,
) -> InitConfig {
    let (nic, dns) = match network {
        HelperNetwork::Disabled => (false, Vec::new()),
        HelperNetwork::TsiUnrestricted => (true, TSI_DNS.to_vec()),
        // eth0 is configured by `init.krun`'s DHCP client; the netstack resolves on the gateway.
        HelperNetwork::Userspace => (true, vec![IpAddr::V4(netstack::GATEWAY_IP)]),
    };
    InitConfig {
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
        mounts: plan.guest_mounts.clone(),
        network: nic,
        dns,
        tools_port: TOOLS_PORT,
        host_time_unix_ms: crate::vm::unix_ms(),
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
            "libkrun helper `{}` is missing or not executable; build it with `cargo build -p \
             sandcrab-backend-libkrun --bin sandcrab-krun` and install it with `sandcrab libkrun \
             install`",
            p.display()
        )))
    }
}

/// The helper must carry `com.apple.security.hypervisor` or `krun_start_enter` fails.
#[cfg(target_os = "macos")]
fn check_entitlement(helper: &Path) -> Result<()> {
    let out = std::process::Command::new("/usr/bin/codesign")
        .args(["-d", "--entitlements", "-", "--xml"])
        .arg(helper)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| krun_err("running codesign", e))?;
    let text = String::from_utf8_lossy(&out.stdout);
    if out.status.success() && text.contains("com.apple.security.hypervisor") {
        Ok(())
    } else {
        Err(SandboxError::InvalidConfig(format!(
            "`{}` lacks com.apple.security.hypervisor; run scripts/sign-krun-helper.sh {}",
            helper.display(),
            helper.display()
        )))
    }
}

/// Linux: a configured cgroup parent must be usable (never silently ignored); without one the
/// helper has no host-side memory/pids cap, which is warned about once per process.
#[cfg(target_os = "linux")]
fn check_cgroup(parent: Option<&Path>) -> Result<()> {
    static WARNED: std::sync::Once = std::sync::Once::new();
    match parent {
        Some(p) => crate::cgroup::check_parent(p),
        None => {
            WARNED.call_once(|| {
                tracing::warn!(
                    "libkrun: no cgroup parent, so the helper (VMM) memory and pids are not \
                     capped on the host; set {} to a delegated cgroup v2 directory \
                     (docs/hosting.md)",
                    crate::config::CGROUP_ENV
                );
            });
            Ok(())
        }
    }
}

#[cfg(target_os = "linux")]
fn check_kvm(device: &Path) -> Result<()> {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(device)
        .map(drop)
        .map_err(|e| {
            let hint = match e.kind() {
                std::io::ErrorKind::NotFound => {
                    "this host has no KVM (bare metal, or a VM with nested virtualization)"
                }
                std::io::ErrorKind::PermissionDenied => {
                    "add the user to the `kvm` group (`sudo usermod -aG kvm $USER`, then log in again)"
                }
                _ => "check the KVM device",
            };
            krun_err(&format!("{} is not usable", device.display()), format!("{e}; {hint}"))
        })
}

#[async_trait]
impl RuntimeBackend for LibkrunBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Libkrun
    }

    fn capabilities(&self) -> BackendCapabilities {
        CAPABILITIES
    }

    fn enforces_network_policy(&self, network: &NetworkConfig) -> bool {
        net::enforces(network, self.net_options())
    }

    fn network_policy_hint(&self, network: &NetworkConfig) -> Option<String> {
        match net::plan(network, self.net_options()) {
            Err(SandboxError::PolicyUnenforceable(why)) => Some(why),
            _ => None,
        }
    }

    async fn spawn(&self, spec: &ResolvedSpec) -> Result<Box<dyn SandboxHandle>> {
        spec.reject_secrets()?;
        // Fail before creating anything: policy, ports, sizes.
        net::plan(&spec.network, self.net_options())?;
        ports::check(&spec.ports)?;
        vm_size(spec.resources.vcpu, spec.resources.memory_mb)?;

        let (dir, meta) = reap::create_run_dir(&self.config.run_dir, spec.id)?;
        let mut vm = KrunVm::new(spec.id, dir, meta);
        let started = async {
            self.spawn_vm(spec, &mut vm).await?;
            let connector = Arc::new(vm.connector());
            // Published ports: 127.0.0.1 listeners → `tunnel.open` over v.sock (§9).
            let ports =
                ports::publish(Arc::clone(&connector), &spec.ports, vm.exit_watch()).await?;
            if !ports.is_empty() {
                spec.timings.mark_custom("ports_published");
            }
            Ok::<_, SandboxError>((connector, ports))
        }
        .await;
        match started {
            Ok((connector, ports)) => Ok(Box::new(KrunSandbox {
                ports,
                connector,
                vm,
                timings: spec.timings.clone(),
                config: Arc::clone(&self.config),
            })),
            Err(e) => {
                if vm.launched() {
                    vm.kill_and_wait().await;
                }
                vm.cleanup();
                tracing::debug!(sandbox = %spec.id, backend = BACKEND, error = %e, "spawn failed");
                Err(e)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use sandcrab_core::{
        LifecycleTimings, MountMode, PortSpec, ResolvedMount, ResourceLimits, SandboxId, Template,
        TemplateManifest, WorkspaceBacking, WorkspaceId,
    };
    use sandcrab_protocol::init::GuestMount;

    use super::*;
    use crate::helper_proto::{HelperConfigError, VirtiofsSemantics};

    const SHA: &str = "3c2ffda8f326e6cdcbb504d8e7dc450f84dca8c57b001fce7b6bc6dbdaca5b5a";
    const MANIFEST: &str = r#"
[template]
name = "base"
version = "0.1.0"
description = "test"
image = "sandcrab/base:0.1.0"
"#;

    fn libs() -> KrunLibs {
        KrunLibs {
            libkrun: PinnedFile {
                path: "/h/.sandcrab/artifacts/libkrun/1.19.8/libkrun.1.dylib".into(),
                sha256: SHA.into(),
            },
            libkrunfw: PinnedFile {
                path: "/h/.sandcrab/artifacts/libkrun/1.19.8/libkrunfw.5.dylib".into(),
                sha256: SHA.into(),
            },
            netstack: None,
        }
    }

    fn libs_with_netstack() -> KrunLibs {
        KrunLibs {
            netstack: Some(NetstackBin {
                kind: NetstackKind::Gvproxy,
                binary: PinnedFile {
                    path: "/h/.sandcrab/artifacts/libkrun-net/gvproxy-0.9.0/gvproxy".into(),
                    sha256: SHA.into(),
                },
            }),
            ..libs()
        }
    }

    fn spec() -> ResolvedSpec {
        let manifest = TemplateManifest::from_toml_str(MANIFEST).unwrap();
        ResolvedSpec {
            id: SandboxId::new(),
            image: manifest.image().to_owned(),
            image_sha256: None,
            template: Template {
                manifest,
                dir: PathBuf::from("/templates/base"),
            },
            mounts: vec![ResolvedMount {
                workspace_id: WorkspaceId::new(),
                workspace_name: "proj".into(),
                backing: WorkspaceBacking::HostDir("/Users/u/proj".into()),
                host_path: "/Users/u/proj".into(),
                guest_path: "/workspace".into(),
                mode: MountMode::ReadWrite,
                generation: 1,
                root_identity: None,
            }],
            default_cwd: PathBuf::from("/workspace"),
            env: BTreeMap::from([("A".to_owned(), "1".to_owned())]),
            network: NetworkConfig::disabled(),
            ports: Vec::new(),
            user: None,
            resources: ResourceLimits {
                vcpu: 2,
                memory_mb: 512,
                disk_mb: 2048,
            },
            ttl: None,
            secrets: Vec::new(),
            timings: LifecycleTimings::start(),
        }
    }

    fn plan() -> MountPlan {
        workspace::plan_with(&spec().mounts, |_| {
            Ok(crate::helper_proto::HostIdentity { dev: 1, ino: 2 })
        })
        .unwrap()
    }

    fn backend(cfg: LibkrunConfig) -> LibkrunBackend {
        LibkrunBackend::new_unchecked(cfg, libs()).unwrap()
    }

    fn config() -> LibkrunConfig {
        LibkrunConfig::with_home(Path::new("/h/.sandcrab"), "/h/crabd")
    }

    #[test]
    fn capabilities_are_honest() {
        let b = backend(config());
        assert_eq!(b.kind(), BackendKind::Libkrun);
        let c = b.capabilities();
        assert!(c.hardware_isolated);
        assert!(!c.multi_tenant_safe);
        assert!(!c.supports_snapshots);
        assert!(c.supports_host_dir_mounts);
    }

    #[test]
    fn policy_matrix_follows_the_netstack_and_the_tsi_flag() {
        let b = backend(config());
        assert!(b.enforces_network_policy(&NetworkConfig::disabled()));
        assert!(!b.enforces_network_policy(&NetworkConfig::unrestricted()));
        let hint = b
            .network_policy_hint(&NetworkConfig::unrestricted())
            .unwrap();
        assert!(hint.contains("libkrun install"), "{hint}");
        assert!(!b.enforces_network_policy(&NetworkConfig::egress_restricted(["a.com"])));

        let b = LibkrunBackend::new_unchecked(config(), libs_with_netstack()).unwrap();
        assert!(b.enforces_network_policy(&NetworkConfig::unrestricted()));
        assert!(
            b.network_policy_hint(&NetworkConfig::unrestricted())
                .is_none()
        );
        assert!(!b.enforces_network_policy(&NetworkConfig::egress_restricted(["a.com"])));

        let mut cfg = config();
        cfg.allow_tsi_unrestricted = true;
        let b = backend(cfg);
        assert!(b.enforces_network_policy(&NetworkConfig::unrestricted()));
        assert!(!b.enforces_network_policy(&NetworkConfig::egress_restricted(["a.com"])));
    }

    #[tokio::test]
    async fn spawn_refuses_before_creating_anything() {
        // Short base dir: the macOS $TMPDIR is too long for the socket path budget.
        let tmp = tempfile::Builder::new().tempdir_in("/tmp").unwrap();
        let mut cfg = config();
        cfg.run_dir = tmp.path().join("krun");
        let b = backend(cfg);

        let mut s = spec();
        s.network = NetworkConfig::unrestricted();
        let err = b.spawn(&s).await.err().unwrap();
        assert!(matches!(err, SandboxError::PolicyUnenforceable(_)), "{err}");

        let mut s = spec();
        s.network = NetworkConfig::egress_restricted(["example.com"]);
        let err = b.spawn(&s).await.err().unwrap();
        assert!(matches!(err, SandboxError::PolicyUnenforceable(_)), "{err}");

        let mut s = spec();
        s.ports = vec![PortSpec { guest: 8080 }, PortSpec { guest: 8080 }];
        let err = b.spawn(&s).await.err().unwrap();
        assert!(matches!(err, SandboxError::InvalidConfig(_)), "{err}");
        s.ports = vec![PortSpec { guest: 0 }];
        let err = b.spawn(&s).await.err().unwrap();
        assert!(matches!(err, SandboxError::InvalidConfig(_)), "{err}");

        let mut s = spec();
        s.resources.vcpu = 256;
        assert!(matches!(
            b.spawn(&s).await.err().unwrap(),
            SandboxError::InvalidConfig(_)
        ));

        assert!(!tmp.path().join("krun").exists(), "no run dir on refusal");
    }

    #[test]
    fn vm_size_bounds() {
        assert_eq!(vm_size(2, 512).unwrap(), (2, 512));
        assert_eq!(vm_size(255, 128).unwrap(), (255, 128));
        assert!(vm_size(0, 512).is_err());
        assert!(vm_size(256, 512).is_err());
        assert!(vm_size(1, 127).is_err());
        assert!(vm_size(1, u64::from(u32::MAX) + 1).is_err());
    }

    #[test]
    fn user_defaults_to_host() {
        let host = UserSpec::new(501, 20);
        let cfg_default = UserSpec::new(1000, 1000);
        let spec_user = UserSpec::new(0, 0);
        assert_eq!(effective_user(None, None, host), host);
        assert_eq!(effective_user(None, Some(cfg_default), host), cfg_default);
        assert_eq!(
            effective_user(Some(spec_user), Some(cfg_default), host),
            spec_user
        );
    }

    fn inputs<'a>(
        cfg: &'a LibkrunConfig,
        libs: &'a KrunLibs,
        plan: &'a MountPlan,
    ) -> HelperInputs<'a> {
        let run = cfg.sandbox_run_dir("00000000-0000-0000-0000-000000000000");
        HelperInputs {
            libs,
            vcpus: 2,
            memory_mib: 512,
            root: root_disk("/h/.sandcrab/cache/rootfs/k/rootfs.ext4".into()),
            scratch: run.join(SCRATCH_FILE),
            plan,
            config: cfg,
            control_socket: run.join("ctl.sock"),
            tools_socket: run.join("v.sock"),
            network: HelperNetwork::Disabled,
            netstack: None,
        }
    }

    #[test]
    fn helper_config_is_valid_and_tsi_free() {
        let (cfg, libs, plan) = (config(), libs(), plan());
        let h = build_helper_config(&inputs(&cfg, &libs, &plan));
        h.validate().unwrap();
        let line = h.to_line().unwrap();
        let back = HelperConfig::read_from(std::io::Cursor::new(&line)).unwrap();
        assert_eq!(back, h);
        assert_eq!(h.network, HelperNetwork::Disabled);
        assert_eq!(h.root.block_id, ROOT_DRIVE_ID);
        assert!(h.root.read_only);
        assert_eq!(h.scratch.block_id, SCRATCH_DRIVE_ID);
        assert!(!h.scratch.read_only);
        assert_eq!(h.virtiofs.len(), 1);
        assert_eq!(h.virtiofs[0].tag, "ws0");
        assert_eq!(h.virtiofs[0].host_path, Path::new("/Users/u/proj"));
        assert!(!h.virtiofs[0].read_only);
        assert_eq!(h.virtiofs_semantics, VirtiofsSemantics::LinuxComplete);
        assert_eq!(h.exec, GuestExec::default(), "argv/env stay fixed");
        assert_eq!(h.libkrun, libs.libkrun);
        let v: serde_json::Value = serde_json::from_slice(&line).unwrap();
        assert!(v.get("port_map").is_none());
    }

    #[test]
    fn helper_config_carries_tsi_only_when_planned() {
        let (cfg, libs, plan) = (config(), libs(), plan());
        let mut i = inputs(&cfg, &libs, &plan);
        i.network = HelperNetwork::TsiUnrestricted;
        let h = build_helper_config(&i);
        assert_eq!(h.network, HelperNetwork::TsiUnrestricted);
        assert!(h.netstack.is_none());
        h.validate().unwrap();
    }

    #[test]
    fn helper_config_carries_the_netstack_for_userspace() {
        let (cfg, libs, plan) = (config(), libs_with_netstack(), plan());
        let mut i = inputs(&cfg, &libs, &plan);
        i.network = HelperNetwork::Userspace;
        i.netstack = Some(NetstackSpec {
            kind: NetstackKind::Gvproxy,
            binary: libs.netstack.clone().unwrap().binary,
            deny: vec!["203.0.113.9/32".parse().unwrap()],
        });
        let h = build_helper_config(&i);
        h.validate().unwrap();
        let ns = h.netstack.as_ref().unwrap();
        assert_eq!(ns.kind, NetstackKind::Gvproxy);
        assert_eq!(ns.deny.len(), 1);
        // `userspace` without a netstack never validates (no silent TSI, no NIC-less VM).
        i.netstack = None;
        assert!(build_helper_config(&i).validate().is_err());
    }

    #[test]
    fn helper_config_rejects_overlong_sockets() {
        let (cfg, libs, plan) = (config(), libs(), plan());
        let mut i = inputs(&cfg, &libs, &plan);
        i.control_socket = PathBuf::from(format!("/{}", "s".repeat(120)));
        let err = build_helper_config(&i).to_line().unwrap_err();
        assert!(matches!(err, HelperConfigError::Invalid { .. }), "{err}");
    }

    #[test]
    fn init_config_matches_the_plan() {
        let s = spec();
        let p = plan();
        let user = UserSpec::new(501, 20);
        let c = init_config(&s, user, &p, HelperNetwork::Disabled);
        assert_eq!((c.user.uid, c.user.gid), (501, 20));
        assert_eq!(
            c.mounts,
            [GuestMount::virtio_fs("ws0", "/workspace", false)]
        );
        assert!(!c.network && c.dns.is_empty());
        assert_eq!(c.tools_port, TOOLS_PORT);
        assert_eq!(c.env["A"], "1");
        assert!(c.hostname.starts_with("sc-") && c.hostname.len() == 11);
        let c = init_config(&s, user, &p, HelperNetwork::TsiUnrestricted);
        assert!(c.network);
        assert_eq!(c.dns, TSI_DNS);
        let c = init_config(&s, user, &p, HelperNetwork::Userspace);
        assert!(c.network);
        assert_eq!(c.dns, [IpAddr::V4(netstack::GATEWAY_IP)]);
    }

    #[test]
    fn libs_hash_and_check() {
        let tmp = tempfile::tempdir().unwrap();
        let (a, b) = (
            tmp.path().join("libkrun.so.1"),
            tmp.path().join("libkrunfw.so.5"),
        );
        std::fs::write(&a, "krun").unwrap();
        std::fs::write(&b, "krunfw").unwrap();
        let mut libs = KrunLibs::hash_files(&a, &b).unwrap();
        assert!(libs.netstack.is_none());
        libs.check(true).unwrap();
        let ns = tmp.path().join("passt");
        std::fs::write(&ns, "passt").unwrap();
        libs.netstack = Some(NetstackBin {
            kind: NetstackKind::Passt,
            binary: PinnedFile {
                path: ns.clone(),
                sha256: sandcrab_rootfs::hash::sha256_file(&ns).unwrap(),
            },
        });
        libs.check(true).unwrap();
        std::fs::write(&ns, "evil").unwrap();
        assert!(matches!(
            libs.check(true).unwrap_err(),
            SandboxError::DigestMismatch { .. }
        ));
        libs.netstack = None;
        assert_eq!(libs.libkrunfw_dir(), tmp.path());
        std::fs::write(&b, "tampered").unwrap();
        assert!(matches!(
            libs.check(true).unwrap_err(),
            SandboxError::DigestMismatch { .. }
        ));
        libs.check(false).unwrap();
        std::fs::remove_file(&a).unwrap();
        assert!(libs.check(false).is_err());
    }

    #[test]
    fn new_requires_an_executable_helper() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = config();
        cfg.helper = tmp.path().join("sandcrab-krun");
        let err = LibkrunBackend::new(cfg, libs()).unwrap_err().to_string();
        assert!(err.contains("not executable"), "{err}");
    }
}
