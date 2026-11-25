//! [`DockerBackend`]: spawn / destroy hardened containers through `bollard`, talking to
//! the real `crabd` over a host-side Unix socket or (Docker Desktop) a loopback TCP relay,
//! always with a per-sandbox auth token. Never `docker exec`.

use std::fs;
use std::io::{self, Write as _};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bollard::Docker;
use bollard::errors::Error as DockerError;
use bollard::models::PortMap;
use bollard::query_parameters::{
    CreateContainerOptionsBuilder, RemoveContainerOptionsBuilder, RemoveVolumeOptions,
    StopContainerOptionsBuilder,
};
use sandcrab_core::template::ImageArch;
use sandcrab_core::{
    AuthConnector, BackendCapabilities, BackendKind, Connector, Endpoint, ImageVerifier,
    LifecyclePhase, LifecycleTimings, NetworkConfig, NetworkPolicy, PinStatus, PublishedPort,
    ResolvedSpec, Result, RuntimeBackend, SandboxError, SandboxHandle, SandboxId, TemplateManifest,
    UserSpec, generate_auth_token, verify_image_pin,
};
use sandcrab_protocol::AuthToken;

use crate::spec::{
    RELAY_NETWORK, RELAY_PORT, SOCKET_NAME, SpecContext, TOKEN_NAME, build_container_spec,
    build_relay_spec, check_relay_network, container_name, effective_user, network_mode,
    relay_name, relay_network_spec, sock_volume_name, sock_volume_spec,
};
use crate::{DockerConfig, DockerTransport, check_static_crabd};

const BACKEND: &str = "docker";
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

/// Docker backend: one hardened container per sandbox with the static `crabd`
/// bind-mounted in, reached over a Unix socket in a per-sandbox host directory
/// ([`DockerTransport::Unix`]) or, on Docker Desktop, through a loopback-only TCP relay
/// container ([`DockerTransport::Tcp`]). Every connection carries a per-sandbox auth token.
///
/// An officially supported production backend for **single-tenant or trusted-host**
/// workloads. It **shares the host kernel**, so it honestly reports
/// `hardware_isolated = false` and `multi_tenant_safe = false`: running mutually hostile
/// tenants' code needs a microVM backend (e.g. Firecracker), which callers can enforce with
/// `require_hardware_isolation`. It must be selected explicitly and is never a fallback
/// for a microVM backend.
#[derive(Debug, Clone)]
pub struct DockerBackend {
    pub(crate) config: DockerConfig,
    pub(crate) docker: Docker,
    /// Canonical, verified static `crabd`.
    crabd: PathBuf,
    host_user: UserSpec,
}

impl DockerBackend {
    /// Fails with [`SandboxError::InvalidConfig`] if `crabd_path` is not a static ELF for the
    /// host architecture. Does not contact the daemon (see [`Self::ping`]).
    pub fn new(config: DockerConfig) -> Result<Self> {
        // A typo in the override must not silently pick the other transport.
        DockerTransport::from_env()?;
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
    pub fn config(&self) -> &DockerConfig {
        &self.config
    }

    /// Host ↔ `crabd` transport this backend uses.
    #[must_use]
    pub fn transport(&self) -> DockerTransport {
        self.config.transport
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

    /// Resolve the template image to its local ID and check it against the host-local pin
    /// the manager resolved (`spec.image_sha256`).
    async fn verify_image(&self, spec: &ResolvedSpec) -> Result<String> {
        let manifest = &spec.template.manifest;
        let status = verify_image_pin(
            &self.image_verifier(),
            manifest.image(),
            spec.image_sha256.as_deref(),
        )
        .await
        .map_err(|e| match e {
            SandboxError::DigestMismatch {
                image,
                expected,
                actual,
            } => SandboxError::DigestMismatch {
                image,
                expected,
                // Renders as: … expected <pin>, got <id> (<hint>)
                actual: format!(
                    "{actual} ({})",
                    sandcrab_core::pins::mismatch_hint(&manifest.template.name)
                ),
            },
            other => other,
        })?;
        match status {
            PinStatus::Verified { digest } => Ok(digest),
            PinStatus::Unpinned { digest } if self.config.allow_unpinned_images => {
                tracing::warn!(
                    template = %manifest.template.name,
                    %digest,
                    "running a template image with no host-local pin; \
                     allowed by DockerConfig::allow_unpinned_images"
                );
                Ok(digest)
            }
            PinStatus::Unpinned { digest } => Err(unpinned_image(manifest, &digest)),
        }
    }

    /// Create `<state_dir>/<id>/` (0700; 0777 when the container runs as another uid so
    /// it can create its socket — `state_dir` itself stays 0700) and write the auth token
    /// into it.
    fn prepare_socket_dir(&self, spec: &ResolvedSpec, token: &AuthToken) -> Result<PathBuf> {
        let dir = self.socket_dir(spec.id);
        let socket = dir.join(SOCKET_NAME);
        let len = socket.as_os_str().len();
        if self.transport() == DockerTransport::Unix && len > MAX_SOCKET_PATH {
            return Err(SandboxError::InvalidConfig(format!(
                "socket path `{}` is {len} bytes (max {MAX_SOCKET_PATH}); use a shorter \
                 DockerConfig::state_dir",
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
        secure_state_dir(state, self.host_user.uid)?;
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&dir)
            .map_err(|e| io_at(&dir, e))?;
        let same_uid = effective_user(spec, self.host_user).uid == self.host_user.uid;
        let mode = if same_uid { 0o700 } else { 0o777 };
        fs::set_permissions(&dir, fs::Permissions::from_mode(mode)).map_err(|e| io_at(&dir, e))?;
        // 0400 for the host uid (= container uid). A different container uid needs 0444;
        // the 0700 `state_dir` (checked by `secure_state_dir`) still keeps other host users out.
        write_token(
            &dir.join(TOKEN_NAME),
            token,
            if same_uid { 0o400 } else { 0o444 },
        )?;
        Ok(dir)
    }

    async fn create_and_start(
        &self,
        spec: &ResolvedSpec,
        objects: &SandboxObjects,
        socket_dir: &Path,
        image_id: &str,
    ) -> Result<(Vec<PublishedPort>, Endpoint)> {
        let owner = self.owner();
        let ctx = SpecContext {
            crabd_path: &self.crabd,
            socket_dir,
            image: image_id,
            host_user: self.host_user,
            limits: &self.config.limits,
            owner: &owner,
            transport: self.transport(),
        };
        let body = build_container_spec(spec, &ctx)?;
        let relay = match &objects.relay {
            Some(_) => Some(build_relay_spec(spec, &ctx)?),
            None => None,
        };
        let timings = &spec.timings;
        let name = objects.container.as_str();
        if let Some(volume) = &objects.volume {
            self.docker
                .create_volume(sock_volume_spec(spec, self.host_user, &owner))
                .await
                .map_err(|e| backend_err(&format!("create volume `{volume}`"), e))?;
            timings.mark_custom("sock_volume_created");
        }
        if objects.relay.is_some() {
            self.ensure_relay_network().await?;
        }
        verify_mount_roots(spec)?;
        // Sandbox and relay are independent until the socket exists: create / start both
        // at once (the host's readiness loop covers the relay coming up first).
        let (main, relay_res) = tokio::join!(self.create(name, body), async {
            match (&objects.relay, relay) {
                (Some(r), Some(body)) => self.create(r, body).await,
                _ => Ok(()),
            }
        });
        main?;
        relay_res?;
        timings.mark_custom("container_created");
        // The daemon resolves bind sources at start: re-check the roots right before.
        verify_mount_roots(spec)?;
        let (main, relay_res) = tokio::join!(self.start(name), async {
            match &objects.relay {
                Some(r) => self.start(r).await,
                None => Ok(()),
            }
        });
        main?;
        relay_res?;
        let started = timings.mark_custom("container_started");
        timings.mark_at(LifecyclePhase::BackendStarted, started);

        let endpoint = match &objects.relay {
            None => Endpoint::Unix(socket_dir.join(SOCKET_NAME)),
            Some(relay_name) => {
                let map = self.port_map(relay_name).await?;
                let port = loopback_port(&map, RELAY_PORT).ok_or_else(|| {
                    backend_err(
                        &format!("relay container `{relay_name}`"),
                        "no 127.0.0.1 host port assigned for the crabd relay",
                    )
                })?;
                timings.mark_custom("relay_port_read");
                Endpoint::Tcp(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port))
            }
        };
        if spec.ports.is_empty() {
            return Ok((Vec::new(), endpoint));
        }
        let ports = self.read_ports(spec, name).await?;
        timings.mark_custom("ports_published");
        Ok((ports, endpoint))
    }

    /// Create [`RELAY_NETWORK`] if missing, and refuse one that exists without inter-container
    /// isolation (never a silent downgrade). Concurrent creators race benignly: a `409` from
    /// create means another spawn won, and the network is then checked like any other.
    async fn ensure_relay_network(&self) -> Result<()> {
        let ctx = format!("relay network `{RELAY_NETWORK}`");
        let info = match self.docker.inspect_network(RELAY_NETWORK, None).await {
            Ok(info) => info,
            Err(e) if status_code(&e) == Some(404) => {
                match self.docker.create_network(relay_network_spec()).await {
                    Ok(_) => {}
                    Err(e) if status_code(&e) == Some(409) => {}
                    Err(e) => return Err(backend_err(&format!("create {ctx}"), e)),
                }
                self.docker
                    .inspect_network(RELAY_NETWORK, None)
                    .await
                    .map_err(|e| backend_err(&format!("inspect {ctx}"), e))?
            }
            Err(e) => return Err(backend_err(&format!("inspect {ctx}"), e)),
        };
        check_relay_network(info.driver.as_deref(), info.options.as_ref()).map_err(|why| {
            SandboxError::InvalidConfig(format!(
                "{ctx} exists but is unsafe for the crabd relay ({why}); remove it \
                 (`docker network rm {RELAY_NETWORK}`) and Sandcrab recreates it with \
                 inter-container traffic off"
            ))
        })
    }

    async fn create(&self, name: &str, body: bollard::models::ContainerCreateBody) -> Result<()> {
        self.docker
            .create_container(
                Some(CreateContainerOptionsBuilder::new().name(name).build()),
                body,
            )
            .await
            .map(|_| ())
            .map_err(|e| backend_err(&format!("create container `{name}`"), e))
    }

    async fn start(&self, name: &str) -> Result<()> {
        self.docker
            .start_container(name, None)
            .await
            .map_err(|e| backend_err(&format!("start container `{name}`"), e))
    }

    async fn port_map(&self, name: &str) -> Result<PortMap> {
        let info = self
            .docker
            .inspect_container(name, None)
            .await
            .map_err(|e| backend_err(&format!("inspect container `{name}`"), e))?;
        Ok(info
            .network_settings
            .and_then(|n| n.ports)
            .unwrap_or_default())
    }

    /// Read back the ephemeral loopback ports Docker assigned.
    async fn read_ports(&self, spec: &ResolvedSpec, name: &str) -> Result<Vec<PublishedPort>> {
        let map = self.port_map(name).await?;
        spec.ports
            .iter()
            .map(|p| {
                let host_port = loopback_port(&map, p.guest).ok_or_else(|| {
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

/// The host port Docker bound on `127.0.0.1` for `guest`/tcp (bindings on any other address
/// are ignored, never used).
pub(crate) fn loopback_port(map: &PortMap, guest: u16) -> Option<u16> {
    map.get(&format!("{guest}/tcp"))
        .and_then(Option::as_ref)
        .into_iter()
        .flatten()
        .filter(|b| b.host_ip.as_deref() == Some("127.0.0.1"))
        .find_map(|b| b.host_port.as_deref()?.parse::<u16>().ok())
}

/// Write the token with `O_CREAT | O_EXCL` and the final `mode` (never briefly wider).
/// `state_dir` is the only thing keeping other host users away from a sandbox's socket dir
/// (0777 when the container runs as another uid) and token (0444 then). So it must be a real
/// directory (not a symlink) owned by this user, and private: a pre-existing, looser one is
/// tightened to 0700, and one owned by someone else is refused.
fn secure_state_dir(state: &Path, uid: u32) -> Result<()> {
    let purpose = format!(
        "DockerConfig::state_dir holds the sandboxes' auth tokens and sockets, so it must be \
         0700 (`{}`)",
        state.display()
    );
    if let Some(t) = sandcrab_core::private_dir::secure_dir(
        state,
        uid,
        sandcrab_core::private_dir::GROUP_WORLD_ANY,
        false,
        &purpose,
    )? {
        tracing::warn!(
            state_dir = %t.path.display(),
            mode = format!("{:o}", t.was),
            "state_dir is accessible to other users; tightened it to 0700"
        );
    }
    Ok(())
}

fn write_token(path: &Path, token: &AuthToken, mode: u32) -> Result<()> {
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(path)
        .map_err(|e| io_at(path, e))?;
    f.write_all(token.expose().as_bytes())
        .map_err(|e| io_at(path, e))?;
    // The umask may have narrowed `mode`; make it exact.
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).map_err(|e| io_at(path, e))
}

/// Docker objects that make up one sandbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SandboxObjects {
    pub container: String,
    /// TCP transport: the relay container.
    pub relay: Option<String>,
    /// TCP transport: the socket volume.
    pub volume: Option<String>,
}

impl SandboxObjects {
    pub(crate) fn new(id: SandboxId, transport: DockerTransport) -> Self {
        let tcp = transport == DockerTransport::Tcp;
        Self {
            container: container_name(id),
            relay: tcp.then(|| relay_name(id)),
            volume: tcp.then(|| sock_volume_name(id)),
        }
    }

    /// Everything a sandbox id may own, whatever transport created it (for reaping).
    pub(crate) fn all(id: SandboxId) -> Self {
        Self::new(id, DockerTransport::Tcp)
    }
}

/// Spawn refusal for a template with no host-local pin (`UnpinnedImage`).
pub(crate) fn unpinned_image(manifest: &TemplateManifest, digest: &str) -> SandboxError {
    SandboxError::Template(format!(
        "{}. Opt out (not recommended) with `--allow-unpinned` \
         (DockerConfig::allow_unpinned_images)",
        sandcrab_core::pins::unpinned_message(manifest, ImageArch::host(), Some(digest))
    ))
}

fn io_at(path: &Path, err: io::Error) -> SandboxError {
    SandboxError::Io(io::Error::new(
        err.kind(),
        format!("{}: {err}", path.display()),
    ))
}

#[async_trait]
impl RuntimeBackend for DockerBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Docker
    }

    /// Shared kernel: hardened, but not a hardware boundary and not safe for mutually
    /// untrusted tenants on one host. Never report more isolation than this.
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
                "docker does not enforce the template's disk_mb limit"
            );
        }

        let token = generate_auth_token()?;
        let socket_dir = self.prepare_socket_dir(spec, &token)?;
        let objects = SandboxObjects::new(spec.id, self.transport());
        match self
            .create_and_start(spec, &objects, &socket_dir, &image_id)
            .await
        {
            Ok((ports, endpoint)) => Ok(Box::new(DockerSandbox {
                id: spec.id,
                docker: self.docker.clone(),
                connector: Arc::new(AuthConnector::new(Arc::new(endpoint), token)),
                objects,
                socket_dir,
                ports,
                timings: spec.timings.clone(),
                stop_grace: self.config.limits.stop_grace,
            })),
            Err(e) => {
                if let Err(ce) = teardown(
                    &self.docker,
                    &objects,
                    Some(&socket_dir),
                    Duration::ZERO,
                    None,
                )
                .await
                {
                    tracing::warn!(name = %objects.container, error = %ce, "cleanup after failed spawn failed");
                }
                Err(e)
            }
        }
    }
}

/// Live Docker sandbox.
#[derive(Debug)]
struct DockerSandbox {
    id: SandboxId,
    docker: Docker,
    objects: SandboxObjects,
    socket_dir: PathBuf,
    /// Authenticating connector; holds the token in memory only (redacted `Debug`).
    connector: Arc<AuthConnector>,
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
        self.connector.clone()
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
            &self.objects,
            Some(&self.socket_dir),
            self.stop_grace,
            Some(&self.timings),
        )
        .await
    }
}

/// Stop the sandbox container (with `grace`) → force-remove it and the relay (with
/// anonymous volumes) → wait until Docker reports them gone → remove the socket volume →
/// remove the socket directory. Idempotent: missing objects are success.
pub(crate) async fn teardown(
    docker: &Docker,
    objects: &SandboxObjects,
    socket_dir: Option<&Path>,
    grace: Duration,
    timings: Option<&LifecycleTimings>,
) -> Result<()> {
    let mark = |name: &str| {
        if let Some(t) = timings {
            t.mark_custom(name);
        }
    };
    let container = objects.container.as_str();
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
    let (main, relay) = tokio::join!(remove_container(docker, container), async {
        match &objects.relay {
            Some(r) => remove_container(docker, r).await,
            None => Ok(()),
        }
    });
    main?;
    mark("container_removed");
    let (main, relay_gone) = tokio::join!(wait_gone(docker, container), async {
        match (&objects.relay, relay) {
            (Some(r), Ok(())) => wait_gone(docker, r).await,
            (_, res) => res,
        }
    });
    main?;
    relay_gone?;
    mark("container_gone");

    if let Some(volume) = &objects.volume {
        match docker
            .remove_volume(volume, None::<RemoveVolumeOptions>)
            .await
        {
            Ok(()) => {}
            Err(e) if status_code(&e) == Some(404) => {}
            Err(e) => return Err(backend_err(&format!("remove volume `{volume}`"), e)),
        }
        mark("sock_volume_removed");
    }

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

/// Force-remove (with anonymous volumes); a missing container is success.
async fn remove_container(docker: &Docker, container: &str) -> Result<()> {
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
        Ok(()) => Ok(()),
        // 404: gone; 409: removal already in progress (wait for it in `wait_gone`).
        Err(e) if matches!(status_code(&e), Some(404 | 409)) => Ok(()),
        Err(e) => Err(backend_err(&format!("remove container `{container}`"), e)),
    }
}

/// Wait until Docker reports `container` gone.
async fn wait_gone(docker: &Docker, container: &str) -> Result<()> {
    let deadline = Instant::now() + GONE_TIMEOUT;
    loop {
        match docker.inspect_container(container, None).await {
            Err(e) if status_code(&e) == Some(404) => return Ok(()),
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

/// Re-check every directory workspace root (symlink-free path, dev/ino from the lease)
/// immediately before Docker uses it as a bind source.
fn verify_mount_roots(spec: &ResolvedSpec) -> Result<()> {
    for m in spec.mounts.iter().filter(|m| m.backing.is_dir()) {
        sandcrab_core::verify_workspace_root(&m.workspace_name, &m.host_path, m.root_identity)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The opt-in is gone, but the isolation claims must stay honest.
    #[test]
    fn capabilities_report_shared_kernel() {
        let backend = DockerBackend {
            config: DockerConfig::default(),
            docker: connect_docker(None).unwrap(),
            crabd: PathBuf::from("/opt/sandcrab/crabd"),
            host_user: UserSpec::new(1000, 1000),
        };
        assert_eq!(backend.kind(), BackendKind::Docker);
        assert_eq!(
            backend.capabilities(),
            BackendCapabilities {
                hardware_isolated: false,
                multi_tenant_safe: false,
                supports_snapshots: false,
                supports_host_dir_mounts: true,
            }
        );
    }

    #[test]
    fn loopback_port_ignores_non_loopback_bindings() {
        use bollard::models::PortBinding;
        use std::collections::HashMap;
        let b = |ip: &str, port: &str| PortBinding {
            host_ip: Some(ip.into()),
            host_port: Some(port.into()),
        };
        let map: PortMap = HashMap::from([
            (
                "7070/tcp".to_owned(),
                Some(vec![b("0.0.0.0", "1111"), b("127.0.0.1", "2222")]),
            ),
            ("8080/tcp".to_owned(), Some(vec![b("0.0.0.0", "3333")])),
            ("9090/tcp".to_owned(), None),
        ]);
        assert_eq!(loopback_port(&map, 7070), Some(2222));
        assert_eq!(loopback_port(&map, 8080), None, "0.0.0.0 is never used");
        assert_eq!(loopback_port(&map, 9090), None);
        assert_eq!(loopback_port(&map, 1), None);
    }

    #[test]
    fn token_file_is_exclusive_and_private() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join(TOKEN_NAME);
        let t = generate_auth_token().unwrap();
        write_token(&p, &t, 0o400).unwrap();
        let meta = fs::metadata(&p).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o400);
        assert_eq!(fs::read_to_string(&p).unwrap(), t.expose());
        // Never overwrites (or follows) an existing file.
        assert!(write_token(&p, &t, 0o400).is_err());
    }

    #[test]
    fn state_dir_is_private_and_owned() {
        let tmp = tempfile::tempdir().unwrap();
        let uid = rustix::process::getuid().as_raw();
        let state = tmp.path().join("run");
        fs::create_dir(&state).unwrap();
        fs::set_permissions(&state, fs::Permissions::from_mode(0o775)).unwrap();
        secure_state_dir(&state, uid).unwrap();
        assert_eq!(
            fs::metadata(&state).unwrap().permissions().mode() & 0o777,
            0o700,
            "a loose state_dir is tightened"
        );
        let e = secure_state_dir(&state, uid + 1).unwrap_err();
        assert!(e.to_string().contains("owned by uid"), "{e}");
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&state, &link).unwrap();
        let e = secure_state_dir(&link, uid).unwrap_err();
        assert!(e.to_string().contains("symlink"), "{e}");
    }

    #[test]
    fn objects_per_transport() {
        let id = SandboxId::new();
        let unix = SandboxObjects::new(id, DockerTransport::Unix);
        assert_eq!(unix.container, container_name(id));
        assert!(unix.relay.is_none() && unix.volume.is_none());
        let tcp = SandboxObjects::new(id, DockerTransport::Tcp);
        assert_eq!(tcp.relay, Some(relay_name(id)));
        assert_eq!(tcp.volume, Some(sock_volume_name(id)));
        assert_eq!(SandboxObjects::all(id), tcp);
    }

    #[test]
    fn unpinned_image_error_names_the_fix() {
        let m = TemplateManifest::from_toml_str(
            "[template]\nname = \"base\"\nversion = \"0.1.0\"\nimage = \"sandcrab/base:0.1.0\"\n",
        )
        .unwrap();
        let e = unpinned_image(&m, "sha256:abc").to_string();
        assert!(e.contains("UnpinnedImage"), "{e}");
        assert!(e.contains("templates/build.sh base"), "{e}");
        assert!(e.contains("sandcrab template pin base"), "{e}");
        assert!(e.contains("not provenance"), "{e}");
        assert!(e.contains("--allow-unpinned"), "{e}");
    }

    #[test]
    fn missing_crabd_is_a_clear_error() {
        let config = DockerConfig {
            crabd_path: "/nonexistent/crabd".into(),
            ..DockerConfig::default()
        };
        let err = DockerBackend::new(config).unwrap_err().to_string();
        assert!(err.contains("scripts/build-crabd.sh"), "{err}");
    }
}
