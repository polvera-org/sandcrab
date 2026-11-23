//! [`DockerDevBackend`]: spawn / destroy hardened containers through `bollard`, talking to
//! the real `crabd` over a host-side Unix socket. Never `docker exec`.

use std::fs;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bollard::Docker;
use bollard::errors::Error as DockerError;
use bollard::query_parameters::{
    CreateContainerOptionsBuilder, RemoveContainerOptionsBuilder, StopContainerOptionsBuilder,
};
use sandcrab_core::{
    BackendCapabilities, BackendKind, Connector, Endpoint, ImageVerifier, LifecyclePhase,
    LifecycleTimings, NetworkConfig, NetworkPolicy, PinStatus, PublishedPort, ResolvedSpec, Result,
    RuntimeBackend, SandboxError, SandboxHandle, SandboxId, UserSpec, verify_template_image,
};

use crate::spec::{
    SOCKET_NAME, SpecContext, build_container_spec, container_name, effective_user, network_mode,
};
use crate::{DockerDevConfig, check_static_crabd};

const BACKEND: &str = "docker-dev";
/// Conservative `sun_path` limit (108 on Linux, 104 on macOS, minus the NUL).
const MAX_SOCKET_PATH: usize = 100;
/// How long `destroy` waits for Docker to report the container gone.
const GONE_TIMEOUT: Duration = Duration::from_secs(10);

pub(crate) fn backend_err(context: &str, err: impl std::fmt::Display) -> SandboxError {
    SandboxError::Backend {
        backend: BACKEND.into(),
        message: format!("{context}: {err}"),
    }
}

/// `tracing` target of the per-spawn "disk_mb is not enforced" warning, so front ends that
/// already print it can filter it out.
pub const DISK_LIMIT_WARN_TARGET: &str = "sandcrab_backend_docker::disk_limit";

pub(crate) fn status_code(err: &DockerError) -> Option<u16> {
    match err {
        DockerError::DockerResponseServerError { status_code, .. } => Some(*status_code),
        _ => None,
    }
}

pub(crate) fn connect_docker(host: Option<&str>) -> Result<Docker> {
    match host {
        Some(h) => Docker::connect_with_host(h),
        None => Docker::connect_with_defaults(),
    }
    .map_err(|e| backend_err("cannot connect to the Docker daemon", e))
}

/// Explicit dev/CI backend: one hardened container per sandbox with the static `crabd`
/// bind-mounted in, reached over a Unix socket in a per-sandbox host directory.
///
/// **Shares the host kernel** — it reports `hardware_isolated = false` and
/// `multi_tenant_safe = false`, and can only be constructed with
/// [`DockerDevConfig::allow_insecure_dev_backend`] set.
#[derive(Debug, Clone)]
pub struct DockerDevBackend {
    pub(crate) config: DockerDevConfig,
    pub(crate) docker: Docker,
    /// Canonical, verified static `crabd`.
    crabd: PathBuf,
    host_user: UserSpec,
}

impl DockerDevBackend {
    /// Fails with [`SandboxError::InsecureBackendRejected`] unless the insecure dev backend
    /// was explicitly allowed, and with [`SandboxError::InvalidConfig`] if `crabd_path` is
    /// not a static ELF for the host architecture. Does not contact the daemon (see
    /// [`Self::ping`]).
    pub fn new(config: DockerDevConfig) -> Result<Self> {
        if !config.allow_insecure_dev_backend {
            return Err(SandboxError::InsecureBackendRejected(
                "the docker-dev backend shares the host kernel and is for local dev/CI only; \
                 set DockerDevConfig::allow_insecure_dev_backend = true to use it explicitly"
                    .into(),
            ));
        }
        let crabd = check_static_crabd(&config.crabd_path)?;
        let docker = connect_docker(config.docker_host.as_deref())?;
        let host_user = UserSpec::new(
            rustix::process::getuid().as_raw(),
            rustix::process::getgid().as_raw(),
        );
        Ok(Self {
            config,
            docker,
            crabd,
            host_user,
        })
    }

    #[must_use]
    pub fn config(&self) -> &DockerDevConfig {
        &self.config
    }

    /// Canonical host path of the verified `crabd` binary.
    #[must_use]
    pub fn crabd_path(&self) -> &Path {
        &self.crabd
    }

    /// Template `disk_mb` is never enforced by this backend (overlay2 size limits need an
    /// xfs + pquota backing filesystem): the container root filesystem is unbounded. Callers
    /// should surface this; [`RuntimeBackend::spawn`] also logs it (target
    /// [`DISK_LIMIT_WARN_TARGET`]).
    #[must_use]
    pub fn enforces_disk_limit(&self) -> bool {
        false
    }

    /// Check the daemon is reachable; returns its version string.
    pub async fn ping(&self) -> Result<String> {
        let v = self
            .docker
            .version()
            .await
            .map_err(|e| backend_err("Docker daemon unreachable", e))?;
        Ok(v.version.unwrap_or_default())
    }

    /// An [`ImageVerifier`] backed by this backend's daemon.
    #[must_use]
    pub fn image_verifier(&self) -> DockerImageVerifier {
        DockerImageVerifier {
            docker: self.docker.clone(),
        }
    }

    pub(crate) fn socket_dir(&self, id: SandboxId) -> PathBuf {
        self.config.state_dir.join(id.to_string())
    }

    /// Resolve the template image to its local ID and check it against the pin.
    async fn verify_image(&self, spec: &ResolvedSpec) -> Result<String> {
        let manifest = &spec.template.manifest;
        let status = verify_template_image(&self.image_verifier(), manifest, false)
            .await
            .map_err(|e| match e {
                SandboxError::DigestMismatch {
                    image,
                    expected,
                    actual,
                } => SandboxError::DigestMismatch {
                    // Renders as: for `<image>` — re-pin with `templates/build.sh --pin <name>`
                    image: format!(
                        "{image}` — rebuild and re-pin with `templates/build.sh --pin {}",
                        manifest.template.name
                    ),
                    expected,
                    actual,
                },
                other => other,
            })?;
        match status {
            PinStatus::Verified { digest } => Ok(digest),
            PinStatus::Unpinned { digest } if self.config.allow_unpinned_images => {
                tracing::warn!(
                    template = %manifest.template.name,
                    %digest,
                    "running an unpinned template image (no sha256 in sandcrab.toml); \
                     allowed by DockerDevConfig::allow_unpinned_images"
                );
                Ok(digest)
            }
            PinStatus::Unpinned { digest } => Err(unpinned_image(
                &manifest.template.name,
                manifest.image(),
                &digest,
            )),
        }
    }

    /// Create `<state_dir>/<id>/` (0700; 0777 when the container runs as another uid so
    /// it can create its socket — `state_dir` itself stays 0700).
    fn prepare_socket_dir(&self, spec: &ResolvedSpec) -> Result<PathBuf> {
        let dir = self.socket_dir(spec.id);
        let socket = dir.join(SOCKET_NAME);
        let len = socket.as_os_str().len();
        if len > MAX_SOCKET_PATH {
            return Err(SandboxError::InvalidConfig(format!(
                "socket path `{}` is {len} bytes (max {MAX_SOCKET_PATH}); use a shorter \
                 DockerDevConfig::state_dir",
                socket.display()
            )));
        }
        let state = &self.config.state_dir;
        if !state.exists() {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(state)
                .map_err(|e| io_at(state, e))?;
        }
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&dir)
            .map_err(|e| io_at(&dir, e))?;
        let mode = if effective_user(spec, self.host_user).uid == self.host_user.uid {
            0o700
        } else {
            0o777
        };
        fs::set_permissions(&dir, fs::Permissions::from_mode(mode)).map_err(|e| io_at(&dir, e))?;
        Ok(dir)
    }

    async fn create_and_start(
        &self,
        spec: &ResolvedSpec,
        name: &str,
        socket_dir: &Path,
        image_id: &str,
    ) -> Result<Vec<PublishedPort>> {
        let body = build_container_spec(
            spec,
            &SpecContext {
                crabd_path: &self.crabd,
                socket_dir,
                image: image_id,
                host_user: self.host_user,
                limits: &self.config.limits,
                owner: &self.owner(),
            },
        )?;
        let timings = &spec.timings;
        self.docker
            .create_container(
                Some(CreateContainerOptionsBuilder::new().name(name).build()),
                body,
            )
            .await
            .map_err(|e| backend_err(&format!("create container `{name}`"), e))?;
        timings.mark_custom("container_created");
        self.docker
            .start_container(name, None)
            .await
            .map_err(|e| backend_err(&format!("start container `{name}`"), e))?;
        let started = timings.mark_custom("container_started");
        timings.mark_at(LifecyclePhase::BackendStarted, started);
        if spec.ports.is_empty() {
            return Ok(Vec::new());
        }
        let ports = self.read_ports(spec, name).await?;
        timings.mark_custom("ports_published");
        Ok(ports)
    }

    /// Read back the ephemeral loopback ports Docker assigned.
    async fn read_ports(&self, spec: &ResolvedSpec, name: &str) -> Result<Vec<PublishedPort>> {
        let info = self
            .docker
            .inspect_container(name, None)
            .await
            .map_err(|e| backend_err(&format!("inspect container `{name}`"), e))?;
        let map = info
            .network_settings
            .and_then(|n| n.ports)
            .unwrap_or_default();
        spec.ports
            .iter()
            .map(|p| {
                let key = format!("{}/tcp", p.guest);
                let host_port = map
                    .get(&key)
                    .and_then(Option::as_ref)
                    .into_iter()
                    .flatten()
                    .filter(|b| b.host_ip.as_deref() == Some("127.0.0.1"))
                    .find_map(|b| b.host_port.as_deref()?.parse::<u16>().ok())
                    .ok_or_else(|| {
                        backend_err(
                            &format!("container `{name}`"),
                            format!("no loopback host port assigned for guest port {}", p.guest),
                        )
                    })?;
                Ok(PublishedPort {
                    guest: p.guest,
                    host: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), host_port),
                })
            })
            .collect()
    }
}

/// Spawn refusal for a template without a `sha256` pin (`UnpinnedImage`).
pub(crate) fn unpinned_image(template: &str, image: &str, digest: &str) -> SandboxError {
    SandboxError::Template(format!(
        "UnpinnedImage: template `{template}` image `{image}` (local {digest}) has no `sha256` \
         pin in sandcrab.toml; pin it with `templates/build.sh --pin {template}`, or opt out \
         with `--allow-unpinned` (DockerDevConfig::allow_unpinned_images)"
    ))
}

fn io_at(path: &Path, err: io::Error) -> SandboxError {
    SandboxError::Io(io::Error::new(
        err.kind(),
        format!("{}: {err}", path.display()),
    ))
}

#[async_trait]
impl RuntimeBackend for DockerDevBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::DockerDev
    }

    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities {
            hardware_isolated: false,
            multi_tenant_safe: false,
            supports_snapshots: false,
            supports_host_dir_mounts: true,
        }
    }

    fn enforces_network_policy(&self, network: &NetworkConfig) -> bool {
        // `none` / default bridge; domain allow-lists are not enforceable.
        matches!(
            network.policy,
            NetworkPolicy::Disabled | NetworkPolicy::Unrestricted
        )
    }

    async fn spawn(&self, spec: &ResolvedSpec) -> Result<Box<dyn SandboxHandle>> {
        spec.reject_secrets()?;
        network_mode(&spec.network)?;
        let image_id = self.verify_image(spec).await?;
        spec.timings.mark_custom("image_verified");
        if !self.enforces_disk_limit() {
            tracing::warn!(
                target: DISK_LIMIT_WARN_TARGET,
                template = %spec.template.name(),
                disk_mb = spec.resources.disk_mb,
                "docker-dev does not enforce the template's disk_mb limit"
            );
        }

        let socket_dir = self.prepare_socket_dir(spec)?;
        let name = container_name(spec.id);
        match self
            .create_and_start(spec, &name, &socket_dir, &image_id)
            .await
        {
            Ok(ports) => Ok(Box::new(DockerSandbox {
                id: spec.id,
                docker: self.docker.clone(),
                endpoint: Arc::new(Endpoint::Unix(socket_dir.join(SOCKET_NAME))),
                name,
                socket_dir,
                ports,
                timings: spec.timings.clone(),
                stop_grace: self.config.limits.stop_grace,
            })),
            Err(e) => {
                if let Err(ce) =
                    teardown(&self.docker, &name, Some(&socket_dir), Duration::ZERO, None).await
                {
                    tracing::warn!(%name, error = %ce, "cleanup after failed spawn failed");
                }
                Err(e)
            }
        }
    }
}

/// Live DockerDev sandbox.
#[derive(Debug)]
struct DockerSandbox {
    id: SandboxId,
    docker: Docker,
    name: String,
    socket_dir: PathBuf,
    endpoint: Arc<Endpoint>,
    ports: Vec<PublishedPort>,
    timings: LifecycleTimings,
    stop_grace: Duration,
}

#[async_trait]
impl SandboxHandle for DockerSandbox {
    fn id(&self) -> SandboxId {
        self.id
    }

    fn connector(&self) -> Arc<dyn Connector> {
        self.endpoint.clone()
    }

    fn published_ports(&self) -> Vec<PublishedPort> {
        self.ports.clone()
    }

    fn timings(&self) -> LifecycleTimings {
        self.timings.clone()
    }

    async fn destroy(self: Box<Self>) -> Result<()> {
        teardown(
            &self.docker,
            &self.name,
            Some(&self.socket_dir),
            self.stop_grace,
            Some(&self.timings),
        )
        .await
    }
}

/// Stop (with `grace`) → force-remove (with anonymous volumes) → wait until Docker reports
/// the container gone → remove the socket directory. Idempotent: a missing container or
/// directory is success.
pub(crate) async fn teardown(
    docker: &Docker,
    container: &str,
    socket_dir: Option<&Path>,
    grace: Duration,
    timings: Option<&LifecycleTimings>,
) -> Result<()> {
    let mark = |name: &str| {
        if let Some(t) = timings {
            t.mark_custom(name);
        }
    };
    if !grace.is_zero() {
        let t = i32::try_from(grace.as_secs()).unwrap_or(i32::MAX);
        match docker
            .stop_container(
                container,
                Some(StopContainerOptionsBuilder::new().t(t).build()),
            )
            .await
        {
            Ok(()) => {}
            // 304: already stopped; 404: already gone.
            Err(e) if matches!(status_code(&e), Some(304 | 404)) => {}
            // Force removal below kills it anyway.
            Err(e) => tracing::warn!(%container, error = %e, "graceful stop failed"),
        }
        mark("container_stopped");
    }
    match docker
        .remove_container(
            container,
            Some(
                RemoveContainerOptionsBuilder::new()
                    .force(true)
                    .v(true)
                    .build(),
            ),
        )
        .await
    {
        Ok(()) => {}
        // 404: gone; 409: removal already in progress (wait for it below).
        Err(e) if matches!(status_code(&e), Some(404 | 409)) => {}
        Err(e) => return Err(backend_err(&format!("remove container `{container}`"), e)),
    }
    mark("container_removed");

    let deadline = Instant::now() + GONE_TIMEOUT;
    loop {
        match docker.inspect_container(container, None).await {
            Err(e) if status_code(&e) == Some(404) => break,
            Err(e) => return Err(backend_err(&format!("inspect container `{container}`"), e)),
            Ok(_) if Instant::now() >= deadline => {
                return Err(SandboxError::Timeout {
                    what: format!("container `{container}` removal"),
                    after: GONE_TIMEOUT,
                });
            }
            Ok(_) => tokio::time::sleep(Duration::from_millis(5)).await,
        }
    }
    mark("container_gone");

    if let Some(dir) = socket_dir {
        match fs::remove_dir_all(dir) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(io_at(dir, e)),
        }
    }
    mark("socket_dir_removed");
    Ok(())
}

/// [`ImageVerifier`] resolving an image reference to its local Docker image ID.
#[derive(Debug, Clone)]
pub struct DockerImageVerifier {
    docker: Docker,
}

impl DockerImageVerifier {
    /// Verifier for the daemon at `docker_host` (`None` = `$DOCKER_HOST` / local default).
    pub fn connect(docker_host: Option<&str>) -> Result<Self> {
        Ok(Self {
            docker: connect_docker(docker_host)?,
        })
    }
}

#[async_trait]
impl ImageVerifier for DockerImageVerifier {
    async fn image_digest(&self, image: &str) -> Result<String> {
        match self.docker.inspect_image(image).await {
            Ok(info) => info
                .id
                .ok_or_else(|| backend_err(&format!("inspect image `{image}`"), "no image ID")),
            Err(e) if status_code(&e) == Some(404) => Err(SandboxError::NotFound {
                kind: "image",
                id: format!(
                    "{image} (build it with `sandcrab template build <name>` or \
                     `templates/build.sh <name>`)"
                ),
            }),
            Err(e) => Err(backend_err(&format!("inspect image `{image}`"), e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insecure_backend_rejected_by_default() {
        let err = DockerDevBackend::new(DockerDevConfig::default()).unwrap_err();
        assert!(
            matches!(err, SandboxError::InsecureBackendRejected(_)),
            "{err}"
        );
    }

    #[test]
    fn unpinned_image_error_names_the_fix() {
        let e = unpinned_image("base", "sandcrab/base:0.1.0", "sha256:abc").to_string();
        assert!(e.contains("UnpinnedImage"), "{e}");
        assert!(e.contains("templates/build.sh --pin base"), "{e}");
        assert!(e.contains("--allow-unpinned"), "{e}");
    }

    #[test]
    fn missing_crabd_is_a_clear_error() {
        let config = DockerDevConfig {
            crabd_path: "/nonexistent/crabd".into(),
            ..DockerDevConfig::allow_insecure()
        };
        let err = DockerDevBackend::new(config).unwrap_err().to_string();
        assert!(err.contains("scripts/build-crabd.sh"), "{err}");
    }
}
