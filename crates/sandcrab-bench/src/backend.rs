//! Backend selection and the backend-specific probes the spec needs (teardown verification,
//! cgroup/process discovery, disk). Everything timed still goes through the public SDK; the
//! probes here only observe.
//!
//! A backend that cannot run on this host (no `/dev/kvm`, not compiled in, daemon down) is
//! returned as `Err(reason)` and recorded as `unavailable` — never substituted.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use sandcrab_sdk::{Sandbox, Sandcrab};

use crate::results::Capabilities;
use crate::sample;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum BackendId {
    Docker,
    Firecracker,
}

impl BackendId {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.trim() {
            "docker" | "docker-dev" => Ok(Self::Docker),
            "firecracker" | "fc" => Ok(Self::Firecracker),
            other => Err(format!(
                "unknown backend `{other}` (expected docker | firecracker)"
            )),
        }
    }

    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Docker => "docker",
            Self::Firecracker => "firecracker",
        }
    }
}

/// Inputs shared by every backend.
#[derive(Debug, Clone)]
pub struct OpenOpts {
    pub state_dir: PathBuf,
    pub templates_dir: PathBuf,
    pub crabd: PathBuf,
}

/// Per-sandbox handles for resource sampling and teardown verification.
#[derive(Debug, Clone, Default)]
pub struct Probe {
    /// Docker container name / full id.
    pub name: String,
    pub container_id: String,
    /// `containerd-shim` (outside the container cgroup).
    pub shim_pid: Option<u32>,
    pub cgroup: Option<PathBuf>,
}

/// One opened backend.
pub struct LiveBackend {
    pub id: BackendId,
    pub sc: Sandcrab,
    pub caps: Capabilities,
    #[cfg(feature = "docker")]
    docker: Option<bollard::Docker>,
}

impl std::fmt::Debug for LiveBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveBackend").field("id", &self.id).finish()
    }
}

fn caps(sc: &Sandcrab) -> Capabilities {
    let c = sc.backend_capabilities();
    Capabilities {
        hardware_isolated: c.hardware_isolated,
        multi_tenant_safe: c.multi_tenant_safe,
        supports_snapshots: c.supports_snapshots,
        supports_host_dir_mounts: c.supports_host_dir_mounts,
    }
}

/// Firecracker artifact paths for the host declaration (empty if not compiled in).
#[must_use]
pub fn fc_paths(opts: &OpenOpts) -> crate::host::FcPaths {
    #[cfg(all(feature = "firecracker", target_os = "linux"))]
    {
        let c = sandcrab_sdk::FirecrackerConfig::with_home(&opts.state_dir, opts.crabd.clone());
        crate::host::FcPaths {
            firecracker_bin: Some(c.firecracker_bin),
            kernel: Some(c.kernel),
        }
    }
    #[cfg(not(all(feature = "firecracker", target_os = "linux")))]
    {
        let _ = opts;
        crate::host::FcPaths::default()
    }
}

pub async fn open(id: BackendId, opts: &OpenOpts) -> Result<LiveBackend, String> {
    let builder = Sandcrab::builder()
        .state_dir(&opts.state_dir)
        .templates_dir(&opts.templates_dir);
    match id {
        BackendId::Docker => open_docker(builder, opts).await,
        BackendId::Firecracker => open_firecracker(builder, opts),
    }
}

#[cfg(feature = "docker")]
async fn open_docker(
    builder: sandcrab_sdk::SandcrabBuilder,
    opts: &OpenOpts,
) -> Result<LiveBackend, String> {
    if !opts.crabd.is_file() {
        return Err(format!(
            "static crabd not found at {} (build it with scripts/build-crabd.sh or pass --crabd)",
            opts.crabd.display()
        ));
    }
    // Shared kernel: the result records the (honest) capabilities.
    let sc = builder
        .docker(sandcrab_sdk::DockerConfig {
            crabd_path: opts.crabd.clone(),
            ..sandcrab_sdk::DockerConfig::default()
        })
        .build()
        .map_err(|e| format!("cannot initialise docker: {e}"))?;
    let docker = bollard::Docker::connect_with_defaults()
        .map_err(|e| format!("cannot connect to the Docker daemon: {e}"))?;
    docker
        .ping()
        .await
        .map_err(|e| format!("Docker daemon not reachable: {e}"))?;
    Ok(LiveBackend {
        id: BackendId::Docker,
        caps: caps(&sc),
        sc,
        docker: Some(docker),
    })
}

#[cfg(not(feature = "docker"))]
async fn open_docker(
    _builder: sandcrab_sdk::SandcrabBuilder,
    _opts: &OpenOpts,
) -> Result<LiveBackend, String> {
    Err("sandcrab-bench was built without the `docker` feature".into())
}

#[cfg(all(feature = "firecracker", target_os = "linux"))]
fn open_firecracker(
    builder: sandcrab_sdk::SandcrabBuilder,
    opts: &OpenOpts,
) -> Result<LiveBackend, String> {
    let kvm = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/kvm");
    if let Err(e) = kvm {
        return Err(format!(
            "/dev/kvm not usable ({e}); Firecracker needs a KVM host (bare metal or a VM \
             with nested virtualization) — TODO: run on kvm-homelab"
        ));
    }
    let config = sandcrab_sdk::FirecrackerConfig::with_home(&opts.state_dir, opts.crabd.clone());
    for p in [&config.kernel, &config.firecracker_bin] {
        if !p.is_file() {
            return Err(format!(
                "pinned Firecracker artifact {} is missing (`sandcrab doctor --fix`)",
                p.display()
            ));
        }
    }
    let sc = builder
        .firecracker(config)
        .build()
        .map_err(|e| format!("cannot initialise firecracker: {e}"))?;
    Ok(LiveBackend {
        id: BackendId::Firecracker,
        caps: caps(&sc),
        sc,
        #[cfg(feature = "docker")]
        docker: None,
    })
}

#[cfg(not(all(feature = "firecracker", target_os = "linux")))]
fn open_firecracker(
    _builder: sandcrab_sdk::SandcrabBuilder,
    _opts: &OpenOpts,
) -> Result<LiveBackend, String> {
    Err("firecracker backend not compiled in (needs Linux and the `firecracker` feature)".into())
}

#[cfg(feature = "docker")]
fn status_code(e: &bollard::errors::Error) -> Option<u16> {
    match e {
        bollard::errors::Error::DockerResponseServerError { status_code, .. } => Some(*status_code),
        _ => None,
    }
}

/// Polling period / timeout for teardown verification (spec: 1 ms, 10 s).
pub const VERIFY_POLL: Duration = Duration::from_millis(1);
pub const VERIFY_TIMEOUT: Duration = Duration::from_secs(10);

impl LiveBackend {
    /// Discover the host-side handles of a live sandbox (outside any timed window).
    pub async fn probe(&self, sb: &Sandbox) -> Option<Probe> {
        #[cfg(feature = "docker")]
        if let Some(docker) = &self.docker {
            let name = sandcrab_sdk::docker::spec::container_name(sb.id());
            let info = docker.inspect_container(&name, None).await.ok()?;
            let container_id = info.id.unwrap_or_default();
            let init_pid = info
                .state
                .and_then(|s| s.pid)
                .and_then(|p| u32::try_from(p).ok())
                .filter(|p| *p > 0);
            return Some(Probe {
                cgroup: sample::cgroup_dir(init_pid, &container_id),
                shim_pid: init_pid.and_then(sample::proc_ppid),
                name,
                container_id,
            });
        }
        // TODO(fc-bench): VMM pid + run dir from the Firecracker backend for RSS/PSS.
        let _ = sb;
        None
    }

    /// Writable-layer bytes of the sandbox (`docker inspect --size` → `SizeRw`).
    pub async fn disk_rw(&self, probe: &Probe) -> Result<u64, String> {
        #[cfg(feature = "docker")]
        if let Some(docker) = &self.docker {
            let opts = bollard::query_parameters::InspectContainerOptionsBuilder::new()
                .size(true)
                .build();
            let info = docker
                .inspect_container(&probe.name, Some(opts))
                .await
                .map_err(|e| format!("inspect --size: {e}"))?;
            return info
                .size_rw
                .and_then(|v| u64::try_from(v).ok())
                .ok_or_else(|| "docker did not report SizeRw".into());
        }
        let _ = probe;
        Err(format!(
            "disk sampling not implemented for {}",
            self.id.label()
        ))
    }

    /// Shared image bytes for a template (`docker image inspect` → `Size`).
    pub async fn disk_shared(&self, image: &str) -> Result<u64, String> {
        #[cfg(feature = "docker")]
        if let Some(docker) = &self.docker {
            let info = docker
                .inspect_image(image)
                .await
                .map_err(|e| format!("image inspect {image}: {e}"))?;
            return info
                .size
                .and_then(|v| u64::try_from(v).ok())
                .ok_or_else(|| "docker did not report the image size".into());
        }
        let _ = image;
        Err(format!(
            "disk sampling not implemented for {}",
            self.id.label()
        ))
    }

    /// Image ID present on the host.
    pub async fn image_id(&self, image: &str) -> Option<String> {
        #[cfg(feature = "docker")]
        if let Some(docker) = &self.docker {
            return docker.inspect_image(image).await.ok()?.id;
        }
        let _ = image;
        None
    }

    /// Poll until the sandbox's host resources are gone (spec §1 `teardown` stop point).
    /// Docker: `inspect` → 404 and the cgroup dir removed.
    pub async fn verify_gone(&self, probe: Option<&Probe>) -> Result<(), String> {
        let Some(probe) = probe else {
            // TODO(fc-bench): VMM pid exited, tap / vsock UDS / API socket / overlay removed.
            return Ok(());
        };
        let deadline = Instant::now() + VERIFY_TIMEOUT;
        loop {
            if self.resources_gone(probe).await? {
                return Ok(());
            }
            if Instant::now() > deadline {
                return Err(format!(
                    "resources of {} ({}) still present after {VERIFY_TIMEOUT:?}",
                    probe.name, probe.container_id
                ));
            }
            tokio::time::sleep(VERIFY_POLL).await;
        }
    }

    async fn resources_gone(&self, probe: &Probe) -> Result<bool, String> {
        let cgroup_gone = probe.cgroup.as_deref().is_none_or(|p| !p.exists());
        #[cfg(feature = "docker")]
        if let Some(docker) = &self.docker {
            let container_gone = match docker.inspect_container(&probe.name, None).await {
                Err(e) if status_code(&e) == Some(404) => true,
                Err(e) => return Err(format!("inspect {}: {e}", probe.name)),
                Ok(_) => false,
            };
            return Ok(container_gone && cgroup_gone);
        }
        Ok(cgroup_gone)
    }
}

/// Default static crabd location (same as the e2e harness).
#[must_use]
pub fn default_crabd(repo_root: &Path) -> PathBuf {
    if let Some(p) = std::env::var_os("SANDCRAB_CRABD").filter(|v| !v.is_empty()) {
        return PathBuf::from(p);
    }
    repo_root.join(format!(
        "target/{}-unknown-linux-musl/release/crabd",
        std::env::consts::ARCH
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_names() {
        assert_eq!(BackendId::parse("docker").unwrap(), BackendId::Docker);
        assert_eq!(BackendId::parse("docker-dev").unwrap(), BackendId::Docker);
        assert_eq!(BackendId::parse("fc").unwrap(), BackendId::Firecracker);
        assert!(BackendId::parse("kata").is_err());
        assert_eq!(BackendId::Firecracker.label(), "firecracker");
    }
}
