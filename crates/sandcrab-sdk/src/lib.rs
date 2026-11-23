//! Ergonomic async Rust SDK for Sandcrab: ephemeral sandboxes, persistent workspaces and
//! the agent tool catalog, all over the same `crabd` protocol on every backend.
//!
//! # Quickstart
//!
//! ```no_run
//! use sandcrab_sdk::{DockerDevConfig, MountMode, NetworkPolicy, SandboxConfig, Sandcrab};
//!
//! # async fn demo() -> sandcrab_sdk::Result<()> {
//! // An explicit backend is required. DockerDev shares the host kernel, so it needs the
//! // explicit insecure opt-in (local dev / CI only).
//! let sandcrab = Sandcrab::builder()
//!     .docker(DockerDevConfig::allow_insecure())
//!     .templates_dir("templates")
//!     .build()?;
//!
//! // Persistent workspace, mutated from the host between agent turns.
//! let ws = sandcrab.workspaces();
//! ws.create("site")?;
//! ws.put_file("site", "index.html", b"<h1>hi</h1>")?;
//!
//! // Ephemeral sandbox with the workspace mounted read-write.
//! let sandbox = sandcrab
//!     .spawn(
//!         SandboxConfig::from_template("ts-dev")
//!             .mount("site", "/workspace", MountMode::ReadWrite)
//!             .default_cwd("/workspace")
//!             .network(NetworkPolicy::Unrestricted)
//!             .port(5173),
//!     )
//!     .await?;
//!
//! let out = sandbox.bash("ls -la && node --version").await?;
//! println!("exit={:?}\n{}", out.exit_code, out.stdout);
//! sandbox.file_edit("index.html", "hi", "hello").await?;
//!
//! let timings = sandbox.destroy().await?;
//! println!("spawn→ready: {:?}", timings.spawn_ready());
//! # Ok(())
//! # }
//! ```
//!
//! # Backends
//!
//! There is no default and no fallback: pick one with [`SandcrabBuilder::docker`] (feature
//! `docker`, on by default) or [`SandcrabBuilder::backend`] for any [`RuntimeBackend`].
//! A backend that is not hardware isolated is refused unless explicitly allowed
//! ([`DockerDevConfig::allow_insecure_dev_backend`] / [`SandcrabBuilder::allow_insecure_backend`]),
//! and [`SandcrabBuilder::require_hardware_isolation`] refuses it outright.
//!
//! # Sandbox lifetime
//!
//! Call [`Sandbox::destroy`]. A [`Sandbox`] dropped without it logs a warning and starts a
//! best-effort background destroy on the current tokio runtime; if there is none (or the
//! runtime shuts down first), the sandbox is torn down by the manager's TTL reaper
//! ([`SandboxConfig::ttl`], default 30 min) or when the [`Sandcrab`] is dropped. Containers
//! leaked by a crashed process are removed by `sandcrab doctor --fix`
//! ([`Sandcrab::reap_orphans`]).

mod sandbox;
mod workspace;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

pub use sandbox::Sandbox;
pub use workspace::{TemplateClient, WorkspaceClient};

pub use sandcrab_core::{
    BackendCapabilities, BackendKind, FileEntry, FileKind, LifecyclePhase, ManagerConfig,
    MountMode, NetworkConfig, NetworkPolicy, PinStatus, PortSpec, PublishedPort, ResourceOverrides,
    Result, RuntimeBackend, SandboxConfig, SandboxError, SandboxId, SandboxInfo, Template,
    TimingMarks, TimingReport, UserSpec, Workspace, WorkspaceBacking, WorkspaceId, WorkspaceMount,
    WorkspaceRef,
};
pub use sandcrab_protocol::{
    BashParams, BashResult, DownloadParams, DownloadResult, FileDeleteParams, FileDeleteResult,
    FileEditParams, FileEditResult, FileMoveParams, FileMoveResult, FileReadParams, FileReadResult,
    FileWriteParams, FileWriteResult, GlobParams, GlobResult, GrepParams, GrepResult, KillSignal,
    ListDirParams, ListDirResult, ProcessKillParams, ProcessKillResult, ProcessListParams,
    ProcessListResult, ProcessPollParams, ProcessPollResult, ProcessStartParams,
    ProcessStartResult, ProcessStdinParams, ProcessStdinResult, ProtocolError, TOOL_NAMES,
    ToolCall, ToolDefinition, ToolResult, UploadParams, UploadResult, tool_definitions,
};

/// The core crate, for lower-level access ([`sandcrab_core::SandboxManager`], ...).
pub use sandcrab_core as core;
/// The wire protocol crate (all tool params/results, framing).
pub use sandcrab_protocol as protocol;

#[cfg(feature = "docker")]
pub use sandcrab_backend_docker::{self as docker, DockerDevBackend, DockerDevConfig, ReapReport};

use sandcrab_core::{SandboxManager, TemplateRegistry, WorkspaceRegistry};

/// Which runtime backend a [`Sandcrab`] runs on. Always explicit.
///
/// New backends are added as a feature-gated variant plus a builder shorthand
/// (e.g. `Firecracker(FirecrackerConfig)` / `.firecracker(..)`).
#[non_exhaustive]
pub enum BackendConfig {
    /// DockerDev (`docker-dev`): shared kernel, local dev / CI only. Construction fails
    /// unless `allow_insecure_dev_backend` is set.
    #[cfg(feature = "docker")]
    Docker(DockerDevConfig),
    /// Any [`RuntimeBackend`]. `allow_insecure` must be true for a backend that is not
    /// hardware isolated.
    Custom {
        backend: Arc<dyn RuntimeBackend>,
        allow_insecure: bool,
    },
}

impl std::fmt::Debug for BackendConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            #[cfg(feature = "docker")]
            Self::Docker(c) => f.debug_tuple("Docker").field(c).finish(),
            Self::Custom {
                backend,
                allow_insecure,
            } => f
                .debug_struct("Custom")
                .field("kind", &backend.kind())
                .field("allow_insecure", allow_insecure)
                .finish(),
        }
    }
}

/// Builder for [`Sandcrab`]. See the crate docs for an example.
#[derive(Debug, Default)]
#[must_use]
pub struct SandcrabBuilder {
    backend: Option<BackendConfig>,
    allow_insecure_custom: bool,
    state_dir: Option<PathBuf>,
    templates_dirs: Vec<PathBuf>,
    require_hardware_isolation: bool,
    ready_timeout: Option<Duration>,
    call_timeout: Option<Duration>,
}

impl SandcrabBuilder {
    /// Use the DockerDev backend. `config.allow_insecure_dev_backend` must be `true`
    /// (e.g. [`DockerDevConfig::allow_insecure`]); it is the explicit insecure opt-in.
    #[cfg(feature = "docker")]
    pub fn docker(mut self, config: DockerDevConfig) -> Self {
        self.backend = Some(BackendConfig::Docker(config));
        self
    }

    /// Use any [`RuntimeBackend`]. Non-hardware-isolated backends additionally need
    /// [`Self::allow_insecure_backend`].
    pub fn backend(mut self, backend: impl RuntimeBackend + 'static) -> Self {
        self.backend = Some(BackendConfig::Custom {
            backend: Arc::new(backend),
            allow_insecure: false,
        });
        self
    }

    /// Select the backend from a [`BackendConfig`].
    pub fn backend_config(mut self, config: BackendConfig) -> Self {
        self.backend = Some(config);
        self
    }

    /// Explicitly allow a custom backend that is not hardware isolated (local dev / CI).
    /// For DockerDev the opt-in is [`DockerDevConfig::allow_insecure_dev_backend`].
    pub fn allow_insecure_backend(mut self, allow: bool) -> Self {
        self.allow_insecure_custom = allow;
        self
    }

    /// Sandcrab state root (workspace registry and, for DockerDev, the per-sandbox socket
    /// dirs under `<state_dir>/run` unless `DockerDevConfig::state_dir` was changed).
    /// Default: `$SANDCRAB_HOME`, else `~/.sandcrab`.
    pub fn state_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.state_dir = Some(dir.into());
        self
    }

    /// Add a template directory (`<dir>/<name>/sandcrab.toml`). May be called repeatedly;
    /// on a name clash the directory added **first** wins. No directory = no templates.
    pub fn templates_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.templates_dirs.push(dir.into());
        self
    }

    /// Refuse backends (and spawns) without hardware isolation.
    pub fn require_hardware_isolation(mut self, required: bool) -> Self {
        self.require_hardware_isolation = required;
        self
    }

    /// Deadline for `crabd` readiness after the backend starts a sandbox (default 30 s).
    pub fn ready_timeout(mut self, timeout: Duration) -> Self {
        self.ready_timeout = Some(timeout);
        self
    }

    /// Default per-tool-call timeout (default 150 s; `bash` calls with a longer
    /// `timeout_ms` get that plus a grace period).
    pub fn call_timeout(mut self, timeout: Duration) -> Self {
        self.call_timeout = Some(timeout);
        self
    }

    /// Validate the configuration, construct the backend and open the registries.
    pub fn build(self) -> Result<Sandcrab> {
        let backend_config = self.backend.ok_or_else(|| {
            SandboxError::InvalidConfig(
                "no runtime backend configured: call .docker(..) or .backend(..) \
                 explicitly (Sandcrab never picks or falls back to a backend)"
                    .into(),
            )
        })?;

        let state_dir = match self.state_dir {
            Some(dir) => dir,
            None => sandcrab_core::default_state_root()?,
        };

        #[cfg(feature = "docker")]
        let mut docker = None;
        let (backend, allow_insecure): (Arc<dyn RuntimeBackend>, bool) = match backend_config {
            #[cfg(feature = "docker")]
            BackendConfig::Docker(mut config) => {
                if config.state_dir == sandcrab_backend_docker::default_state_dir() {
                    config.state_dir = state_dir.join("run");
                }
                let allow = config.allow_insecure_dev_backend;
                let b = DockerDevBackend::new(config)?;
                docker = Some(b.clone());
                (Arc::new(b), allow)
            }
            BackendConfig::Custom {
                backend,
                allow_insecure,
            } => (backend, allow_insecure || self.allow_insecure_custom),
        };

        if self.require_hardware_isolation && !backend.capabilities().hardware_isolated {
            return Err(SandboxError::InsecureBackendRejected(format!(
                "hardware isolation required but backend `{}` does not provide it",
                backend.kind()
            )));
        }

        let workspaces = WorkspaceRegistry::open(&state_dir)?;
        let templates = TemplateRegistry::load(&self.templates_dirs)?;
        let defaults = ManagerConfig::default();
        let manager = SandboxManager::new(
            backend,
            workspaces,
            templates,
            ManagerConfig {
                allow_insecure_backend: allow_insecure,
                ready_timeout: self.ready_timeout.unwrap_or(defaults.ready_timeout),
                call_timeout: self.call_timeout.unwrap_or(defaults.call_timeout),
                ..defaults
            },
        )?;
        Ok(Sandcrab {
            manager,
            require_hardware_isolation: self.require_hardware_isolation,
            #[cfg(feature = "docker")]
            docker,
        })
    }
}

/// Entry point: one backend, a workspace registry and a template registry.
///
/// Dropping it tears down every sandbox it still tracks (best effort, in the background).
pub struct Sandcrab {
    manager: SandboxManager,
    require_hardware_isolation: bool,
    #[cfg(feature = "docker")]
    docker: Option<DockerDevBackend>,
}

impl std::fmt::Debug for Sandcrab {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sandcrab")
            .field("backend", &self.backend_kind())
            .field("state_dir", &self.state_dir())
            .finish_non_exhaustive()
    }
}

impl Sandcrab {
    pub fn builder() -> SandcrabBuilder {
        SandcrabBuilder::default()
    }

    #[must_use]
    pub fn backend_kind(&self) -> BackendKind {
        self.manager.backend_kind()
    }

    #[must_use]
    pub fn backend_capabilities(&self) -> BackendCapabilities {
        self.manager.backend_capabilities()
    }

    /// Canonical state root.
    #[must_use]
    pub fn state_dir(&self) -> &Path {
        self.manager.workspaces().state_root()
    }

    /// Persistent workspaces (host-side operations).
    #[must_use]
    pub fn workspaces(&self) -> WorkspaceClient {
        WorkspaceClient::from_registry(self.manager.workspaces().clone())
    }

    /// Templates loaded at build time.
    #[must_use]
    pub fn templates(&self) -> TemplateClient {
        TemplateClient::from_registry(self.manager.templates().clone())
    }

    /// Lower-level manager (listing, lookup by id, TTL reaping).
    #[must_use]
    pub fn manager(&self) -> &SandboxManager {
        &self.manager
    }

    /// Spawn a sandbox and wait until its `crabd` answers.
    pub async fn spawn(&self, mut config: SandboxConfig) -> Result<Sandbox> {
        config.require_hardware_isolation |= self.require_hardware_isolation;
        Ok(Sandbox::new(self.manager.spawn(config).await?))
    }

    /// Live sandboxes tracked by this instance, oldest first.
    #[must_use]
    pub fn list_sandboxes(&self) -> Vec<SandboxInfo> {
        self.manager.list_active()
    }

    /// Destroy every live sandbox (returns the first error, after trying all).
    pub async fn shutdown(&self) -> Result<()> {
        self.manager.shutdown().await
    }

    /// The DockerDev backend, when that is the selected backend.
    #[cfg(feature = "docker")]
    #[must_use]
    pub fn docker_backend(&self) -> Option<&DockerDevBackend> {
        self.docker.as_ref()
    }

    /// Check the template's pinned image digest against the image present on the backend
    /// (DockerDev only for now).
    #[cfg(feature = "docker")]
    pub async fn verify_template(&self, name: &str) -> Result<PinStatus> {
        let template = self.manager.templates().get(name)?;
        let docker = self.docker.as_ref().ok_or_else(|| {
            SandboxError::InvalidConfig("template verification needs the docker backend".into())
        })?;
        sandcrab_core::verify_template_image(&docker.image_verifier(), &template.manifest, false)
            .await
    }

    /// Remove Sandcrab-managed DockerDev containers that this instance does not track
    /// (crash / cancelled-spawn leftovers). **Host-wide**: also removes sandboxes owned by
    /// *other* running Sandcrab processes on the same daemon.
    #[cfg(feature = "docker")]
    pub async fn reap_orphans(&self) -> Result<ReapReport> {
        let docker = self.docker.as_ref().ok_or_else(|| {
            SandboxError::InvalidConfig("orphan reaping needs the docker backend".into())
        })?;
        let tracked = self
            .manager
            .list_active()
            .into_iter()
            .map(|i| i.id)
            .collect();
        docker.reap_orphans(&tracked).await
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::fs;

    use sandcrab_core::testing::FakeBackend;

    use super::*;

    pub struct Env {
        pub tmp: tempfile::TempDir,
    }

    impl Env {
        pub fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let d = tmp.path().join("templates/base");
            fs::create_dir_all(&d).unwrap();
            fs::write(
                d.join("sandcrab.toml"),
                "[template]\nname = \"base\"\nversion = \"0.1.0\"\nimage = \"sandcrab/base:0.1.0\"\n\
                 [resources]\nvcpu = 1\nmemory_mb = 512\ndisk_mb = 2048\n",
            )
            .unwrap();
            Self { tmp }
        }

        pub fn builder(&self) -> SandcrabBuilder {
            Sandcrab::builder()
                .state_dir(self.tmp.path().join("state"))
                .templates_dir(self.tmp.path().join("templates"))
                .ready_timeout(Duration::from_secs(2))
        }

        pub fn sandcrab(&self, backend: &FakeBackend) -> Sandcrab {
            self.builder().backend(backend.clone()).build().unwrap()
        }
    }
}

#[cfg(test)]
mod tests {
    use sandcrab_core::testing::{FakeBackend, FakeEvent};

    use super::test_support::Env;
    use super::*;

    #[tokio::test]
    async fn backend_is_required() {
        let env = Env::new();
        let err = env.builder().build().unwrap_err();
        assert!(matches!(err, SandboxError::InvalidConfig(_)), "{err}");
        assert!(err.to_string().contains("explicitly"), "{err}");
    }

    #[tokio::test]
    async fn insecure_backend_needs_explicit_opt_in() {
        let env = Env::new();
        let err = env
            .builder()
            .backend(FakeBackend::insecure())
            .build()
            .unwrap_err();
        assert!(
            matches!(err, SandboxError::InsecureBackendRejected(_)),
            "{err}"
        );

        let sc = env
            .builder()
            .backend(FakeBackend::insecure())
            .allow_insecure_backend(true)
            .build()
            .unwrap();
        assert_eq!(sc.backend_kind(), BackendKind::DockerDev);

        let err = env
            .builder()
            .backend(FakeBackend::insecure())
            .allow_insecure_backend(true)
            .require_hardware_isolation(true)
            .build()
            .unwrap_err();
        assert!(
            matches!(err, SandboxError::InsecureBackendRejected(_)),
            "{err}"
        );
    }

    #[cfg(feature = "docker")]
    #[tokio::test]
    async fn docker_without_opt_in_is_rejected() {
        let env = Env::new();
        let err = env
            .builder()
            .docker(DockerDevConfig::default())
            .build()
            .unwrap_err();
        assert!(
            matches!(err, SandboxError::InsecureBackendRejected(_)),
            "{err}"
        );
    }

    #[tokio::test]
    async fn spawn_typed_call_destroy() {
        let env = Env::new();
        let backend = FakeBackend::new();
        let sc = env.sandcrab(&backend);
        assert_eq!(sc.templates().list().len(), 1);
        let ws = sc.workspaces();
        ws.create("proj").unwrap();

        let sb = sc
            .spawn(
                SandboxConfig::from_template("base")
                    .mount("proj", "/workspace", MountMode::ReadWrite)
                    .network(NetworkPolicy::Disabled)
                    .port(5173),
            )
            .await
            .unwrap();
        assert_eq!(sb.published_ports().len(), 1);
        assert_eq!(sb.tool_definitions().len(), TOOL_NAMES.len());
        assert_eq!(sc.list_sandboxes().len(), 1);

        let out = sb.bash("echo hi").await.unwrap();
        assert_eq!(out.stdout, "echo hi");
        assert_eq!(out.exit_code, Some(0));

        // mock crabd: file_read → NotFound
        let err = sb.file_read("nope.txt").await.unwrap_err();
        assert!(
            matches!(err, SandboxError::Protocol(ProtocolError::NotFound(_))),
            "{err}"
        );

        let id = sb.id();
        let marks = sb.destroy().await.unwrap();
        assert!(marks.spawn_ready().is_some());
        assert!(marks.teardown().is_some());
        assert!(sc.list_sandboxes().is_empty());
        assert_eq!(
            backend.events(),
            vec![FakeEvent::Spawned(id), FakeEvent::Destroyed(id)]
        );
    }

    #[tokio::test]
    async fn require_hardware_isolation_applies_to_spawn() {
        let env = Env::new();
        let backend = FakeBackend::new();
        let sc = env.builder().backend(backend).build().unwrap();
        // hardware isolated fake → fine
        let sb = sc
            .spawn(SandboxConfig::from_template("base").require_hardware_isolation(true))
            .await
            .unwrap();
        sb.destroy().await.unwrap();
    }

    #[tokio::test]
    async fn dropped_sandbox_is_destroyed_in_background() {
        let env = Env::new();
        let backend = FakeBackend::new();
        let sc = env.sandcrab(&backend);
        let sb = sc
            .spawn(SandboxConfig::from_template("base"))
            .await
            .unwrap();
        let id = sb.id();
        drop(sb);
        for _ in 0..100 {
            if backend.events().contains(&FakeEvent::Destroyed(id)) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("dropped sandbox was not destroyed: {:?}", backend.events());
    }
}
