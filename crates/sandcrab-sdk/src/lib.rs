//! Ergonomic async Rust SDK for Sandcrab: ephemeral sandboxes, persistent workspaces and
//! the agent tool catalog, all over the same `crabd` protocol on every backend.
//!
//! # Quickstart
//!
//! ```no_run
//! use sandcrab_sdk::{DockerConfig, MountMode, NetworkPolicy, SandboxConfig, Sandcrab};
//!
//! # async fn demo() -> sandcrab_sdk::Result<()> {
//! // An explicit backend is required (no default, no fallback). Docker: hardened
//! // containers on the shared host kernel, for single-tenant / trusted-host workloads.
//! let sandcrab = Sandcrab::builder()
//!     .docker(DockerConfig::default())
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
//! // LLM tool-use: name + JSON input straight from the model, JSON back.
//! let reply = sandbox
//!     .tool_dispatch("grep", serde_json::json!({ "pattern": "hello" }))
//!     .await
//!     .expect("sandbox is alive");
//! println!("{reply}");
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
//! `docker`, on by default), `SandcrabBuilder::firecracker` (feature `firecracker`, off by
//! default, Linux only; hardware isolated, needs `/dev/kvm`), `SandcrabBuilder::libkrun`
//! (feature `libkrun`, off by default, Apple Silicon macOS + Linux; hardware isolated, needs a
//! signed `sandcrab-krun` helper and the pinned libkrun artifacts) or
//! [`SandcrabBuilder::backend`] for any [`RuntimeBackend`]. A backend that cannot start (e.g. no `/dev/kvm`) is an error,
//! never a reason to pick another one.
//! Docker is an officially supported backend: fast, hardened containers on the shared host
//! kernel. It honestly reports `hardware_isolated = false` and `multi_tenant_safe = false`;
//! for mutually hostile multi-tenant code use a microVM backend and set
//! [`SandcrabBuilder::require_hardware_isolation`] (or
//! [`SandboxConfig::require_hardware_isolation`]), which refuses any backend without
//! hardware isolation instead of downgrading.
//!
//! # Processes, forwards, mounts, labels
//!
//! - [`Sandbox::spawn`] runs a program with an exact argv and environment and gives a
//!   [`Process`] handle (stdin, output with backpressure, cancel, an exit that never fails);
//!   [`Sandbox::exec`] collects a bounded run. See [`process`].
//! - [`SandboxConfig::forward`] plus [`Sandbox::serve_forwards`] let programs in a sandbox
//!   with no network reach targets the host chooses. See [`forward`].
//! - [`SandboxConfig::host_mount`] mounts a host directory at the same path, only beneath the
//!   roots of [`SandcrabBuilder::mount_policy`]. Without a policy every host mount is refused.
//! - [`SandboxConfig::label`] labels a sandbox; [`Sandcrab::list`] and
//!   [`Sandcrab::destroy_by_label`] select by label. [`Sandbox::set_ttl`] renews a TTL.
//!
//! A backend that cannot do one of these refuses the spawn with an explicit error
//! ([`BackendCapabilities`] says what it supports); nothing is silently dropped.
//!
//! # Sandbox lifetime
//!
//! Call [`Sandbox::destroy`]. A [`Sandbox`] dropped without it logs a warning and starts a
//! best-effort background destroy on the current tokio runtime; if there is none (or the
//! runtime shuts down first), the sandbox is torn down by the manager's TTL reaper
//! ([`SandboxConfig::ttl`], default 30 min) or when the [`Sandcrab`] is dropped. Containers
//! leaked by a crashed process are removed by `sandcrab doctor --fix`
//! ([`Sandcrab::reap_orphans`]).

mod dispatch;
pub mod forward;
pub mod process;
mod sandbox;
mod workspace;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

pub use dispatch::{ToolError, is_tool_error};
pub use process::{
    DEFAULT_EXEC_OUTPUT_CAP, DEFAULT_EXEC_TIMEOUT, ExecOutput, Output, OutputStream, Process,
    ProcessOutcome, ProcessSpec,
};
pub use sandbox::Sandbox;
pub use workspace::{TemplateClient, WorkspaceClient};

pub use sandcrab_core::{
    AsyncStream, BackendCapabilities, BackendKind, BoxedStream, DestroyByLabel, FileEntry,
    FileKind, ForwardSpec, ForwardTarget, HostMount, LifecyclePhase, ManagerConfig, MountMode,
    MountPolicy, MountRoot, NetworkConfig, NetworkPolicy, NprocScope, PinStatus, PinStore,
    PortSpec, PublishedPort, ResourceOverrides, Result, RuntimeBackend, SandboxConfig,
    SandboxError, SandboxFilter, SandboxId, SandboxInfo, Template, TimingMarks, TimingReport,
    UserSpec, Workspace, WorkspaceBacking, WorkspaceId, WorkspaceMount, WorkspaceRef,
};
pub use sandcrab_protocol::{
    BashParams, BashResult, DownloadParams, DownloadResult, ExecLimits, FileDeleteParams,
    FileDeleteResult, FileEditParams, FileEditResult, FileIdentity, FileMoveParams, FileMoveResult,
    FileReadParams, FileReadResult, FileWriteParams, FileWriteResult, ForwardPortStats,
    ForwardRefused, GlobParams, GlobResult, GrepParams, GrepResult, KillSignal, ListDirParams,
    ListDirResult, OutputCap, Overflow, ProcessCancelResult, ProcessExit, ProcessKillParams,
    ProcessKillResult, ProcessListParams, ProcessListResult, ProcessPollParams, ProcessPollResult,
    ProcessStartParams, ProcessStartResult, ProcessStdinParams, ProcessStdinResult, ProtocolError,
    SpawnTarget, StderrMode, StdinMode, TOOL_NAMES, ToolCall, ToolDefinition, ToolResult,
    UploadParams, UploadResult, tool_definitions,
};

/// The core crate, for lower-level access ([`sandcrab_core::SandboxManager`], ...).
pub use sandcrab_core as core;
/// The wire protocol crate (all tool params/results, framing).
pub use sandcrab_protocol as protocol;

#[cfg(feature = "docker")]
pub use sandcrab_backend_docker::{
    self as docker, DockerBackend, DockerConfig, ReapReport, ReapScope,
};
#[cfg(feature = "docker")]
#[allow(deprecated)]
pub use sandcrab_backend_docker::{DockerDevBackend, DockerDevConfig};

/// The Firecracker backend crate (config types, artifacts, rootfs pipeline, orphan reaping).
#[cfg(all(feature = "firecracker", target_os = "linux"))]
pub use sandcrab_backend_firecracker::{
    self as firecracker, FirecrackerBackend, FirecrackerConfig, RootfsSource,
};

/// The libkrun backend crate (config, pinned artifacts, helper protocol, orphan reaping).
/// Its rootfs source type is `libkrun::RootfsSource` (not re-exported at the root, where
/// `RootfsSource` is Firecracker's).
#[cfg(all(
    feature = "libkrun",
    any(target_os = "linux", all(target_os = "macos", target_arch = "aarch64"))
))]
pub use sandcrab_backend_libkrun::{self as libkrun, KrunLibs, LibkrunBackend, LibkrunConfig};

/// Resolve the pinned libkrun + libkrunfw for `config` (`config.lib_dir`, else
/// `<SANDCRAB_HOME>/artifacts/libkrun/<version>/`; never a system or Homebrew path) and verify
/// the whole provenance chain (`manifest.json` against the compiled-in pins, file SHA-256s).
/// Install them with `sandcrab libkrun install`.
///
/// Also resolves the pinned userspace netstack (passt / gvproxy) for `network.policy =
/// unrestricted` from `<SANDCRAB_HOME>/artifacts/libkrun-net/`: not installed is `None`
/// (that policy is then refused), a tampered or mismatched install is an error.
#[cfg(all(
    feature = "libkrun",
    any(target_os = "linux", all(target_os = "macos", target_arch = "aarch64"))
))]
pub fn libkrun_libs(config: &LibkrunConfig) -> Result<KrunLibs> {
    let a = libkrun::artifacts::Artifacts::resolve(config)?;
    // The stamps verified by `resolve` travel with the digests: the backend and the helper
    // then check each library with one `fstat` (design §12.4).
    Ok(KrunLibs {
        libkrun: a.libkrun.into(),
        libkrunfw: a.libkrunfw.into(),
        netstack: libkrun::netstack_pins::resolve_config(config)?,
    })
}

use sandcrab_core::{SandboxManager, TemplateRegistry, WorkspaceRegistry};

/// Which runtime backend a [`Sandcrab`] runs on. Always explicit.
///
/// New backends are added as a feature-gated variant plus a builder shorthand
/// (e.g. `Firecracker(FirecrackerConfig)` / `.firecracker(..)`).
#[non_exhaustive]
pub enum BackendConfig {
    /// Docker (`docker`): hardened containers on the shared host kernel; supported for
    /// single-tenant or trusted-host workloads (`hardware_isolated = false`).
    #[cfg(feature = "docker")]
    Docker(DockerConfig),
    /// Firecracker microVMs: hardware isolated (KVM). Construction fails without a usable
    /// `/dev/kvm` or the pinned kernel / `firecracker` binary; there is no fallback.
    #[cfg(all(feature = "firecracker", target_os = "linux"))]
    Firecracker(Box<FirecrackerConfig>),
    /// libkrun microVMs: hardware isolated (Hypervisor.framework / KVM). Construction fails
    /// without the signed helper, the pinned libkrun artifacts or (Linux) a usable `/dev/kvm`;
    /// there is no fallback.
    #[cfg(all(
        feature = "libkrun",
        any(target_os = "linux", all(target_os = "macos", target_arch = "aarch64"))
    ))]
    Libkrun(Box<LibkrunConfig>),
    /// Any [`RuntimeBackend`], used exactly as given.
    Custom { backend: Arc<dyn RuntimeBackend> },
}

impl std::fmt::Debug for BackendConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            #[cfg(feature = "docker")]
            Self::Docker(c) => f.debug_tuple("Docker").field(c).finish(),
            #[cfg(all(feature = "firecracker", target_os = "linux"))]
            Self::Firecracker(c) => f.debug_tuple("Firecracker").field(c).finish(),
            #[cfg(all(
                feature = "libkrun",
                any(target_os = "linux", all(target_os = "macos", target_arch = "aarch64"))
            ))]
            Self::Libkrun(c) => f.debug_tuple("Libkrun").field(c).finish(),
            Self::Custom { backend } => f
                .debug_struct("Custom")
                .field("kind", &backend.kind())
                .finish(),
        }
    }
}

/// Builder for [`Sandcrab`]. See the crate docs for an example.
#[derive(Debug, Default)]
#[must_use]
pub struct SandcrabBuilder {
    backend: Option<BackendConfig>,
    state_dir: Option<PathBuf>,
    templates_dirs: Vec<PathBuf>,
    template_pins_dir: Option<PathBuf>,
    mount_policy: Option<MountPolicy>,
    require_hardware_isolation: bool,
    ready_timeout: Option<Duration>,
    call_timeout: Option<Duration>,
}

impl SandcrabBuilder {
    /// Use the Docker backend (shared kernel, hardened containers; see
    /// [`DockerBackend`]). Combine with [`Self::require_hardware_isolation`] to refuse it.
    #[cfg(feature = "docker")]
    pub fn docker(mut self, config: DockerConfig) -> Self {
        self.backend = Some(BackendConfig::Docker(config));
        self
    }

    /// Use the Firecracker microVM backend (hardware isolated).
    /// [`Self::build`] fails if `/dev/kvm` is not usable read-write or the pinned artifacts
    /// are missing / do not match their SHA-256 pins
    /// ([`FirecrackerConfig::fetch_pinned_artifacts`] downloads them).
    #[cfg(all(feature = "firecracker", target_os = "linux"))]
    pub fn firecracker(mut self, config: FirecrackerConfig) -> Self {
        self.backend = Some(BackendConfig::Firecracker(Box::new(config)));
        self
    }

    /// Use the libkrun microVM backend (hardware isolated; Apple Silicon macOS and Linux KVM).
    /// [`Self::build`] resolves the pinned libkrun + libkrunfw ([`libkrun_libs`]) and fails if
    /// they are missing or do not match their pins, if the `sandcrab-krun` helper is missing
    /// (or, on macOS, lacks `com.apple.security.hypervisor`), or if `/dev/kvm` is not usable
    /// (Linux). Never a fallback to another backend.
    #[cfg(all(
        feature = "libkrun",
        any(target_os = "linux", all(target_os = "macos", target_arch = "aarch64"))
    ))]
    pub fn libkrun(mut self, config: LibkrunConfig) -> Self {
        self.backend = Some(BackendConfig::Libkrun(Box::new(config)));
        self
    }

    /// Use any [`RuntimeBackend`]; its declared capabilities are reported as-is.
    pub fn backend(mut self, backend: impl RuntimeBackend + 'static) -> Self {
        self.backend = Some(BackendConfig::Custom {
            backend: Arc::new(backend),
        });
        self
    }

    /// Select the backend from a [`BackendConfig`].
    pub fn backend_config(mut self, config: BackendConfig) -> Self {
        self.backend = Some(config);
        self
    }

    /// Former opt-in for backends without hardware isolation. No longer needed: this is a
    /// no-op kept for one release. Use [`Self::require_hardware_isolation`] to refuse such
    /// backends.
    #[deprecated(note = "no opt-in is needed any more; this is a no-op")]
    pub fn allow_insecure_backend(self, _allow: bool) -> Self {
        self
    }

    /// Sandcrab state root (workspace registry and, for Docker, the per-sandbox socket
    /// dirs under `<state_dir>/run` unless `DockerConfig::state_dir` was changed).
    /// Default: `$SANDCRAB_HOME`, else `~/.sandcrab`.
    pub fn state_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.state_dir = Some(dir.into());
        self
    }

    /// Host-local template pin store (`sandcrab_core::pins`). Default:
    /// `$SANDCRAB_TEMPLATE_PINS`, else `<$SANDCRAB_HOME or ~/.sandcrab>/template-pins`; it is
    /// host-wide (pins describe the host's image store), so [`Self::state_dir`] does not move it.
    pub fn template_pins_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.template_pins_dir = Some(dir.into());
        self
    }

    /// Add a template directory (`<dir>/<name>/sandcrab.toml`). May be called repeatedly;
    /// on a name clash the directory added **first** wins. No directory = no templates.
    pub fn templates_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.templates_dirs.push(dir.into());
        self
    }

    /// The allowed roots for host mounts ([`SandboxConfig::host_mount`]) and the directories
    /// no mount may touch ([`MountPolicy::new`]). Without a policy every host mount is
    /// refused.
    pub fn mount_policy(mut self, policy: MountPolicy) -> Self {
        self.mount_policy = Some(policy);
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
                "no runtime backend configured: call .docker(..), .firecracker(..), \
                 .libkrun(..) or .backend(..) explicitly (Sandcrab never picks or falls back to a backend)"
                    .into(),
            )
        })?;

        let state_dir = match self.state_dir {
            Some(dir) => dir,
            None => sandcrab_core::default_state_root()?,
        };

        // Statically known not to be hardware isolated: refuse before touching the daemon.
        #[cfg(feature = "docker")]
        if self.require_hardware_isolation && matches!(backend_config, BackendConfig::Docker(_)) {
            return Err(SandboxError::HardwareIsolationRequired(
                "the docker backend shares the host kernel".into(),
            ));
        }

        #[cfg(feature = "docker")]
        let mut docker = None;
        #[cfg(all(feature = "firecracker", target_os = "linux"))]
        let mut firecracker = None;
        #[cfg(all(
            feature = "libkrun",
            any(target_os = "linux", all(target_os = "macos", target_arch = "aarch64"))
        ))]
        let mut libkrun = None;
        let backend: Arc<dyn RuntimeBackend> = match backend_config {
            #[cfg(feature = "docker")]
            BackendConfig::Docker(mut config) => {
                if config.state_dir == sandcrab_backend_docker::default_state_dir() {
                    config.state_dir = state_dir.join("run");
                }
                let b = DockerBackend::new(config)?;
                docker = Some(b.clone());
                Arc::new(b)
            }
            #[cfg(all(feature = "firecracker", target_os = "linux"))]
            BackendConfig::Firecracker(config) => {
                let b = FirecrackerBackend::new(*config)?;
                firecracker = Some(b.clone());
                Arc::new(b)
            }
            #[cfg(all(
                feature = "libkrun",
                any(target_os = "linux", all(target_os = "macos", target_arch = "aarch64"))
            ))]
            BackendConfig::Libkrun(config) => {
                let libs = libkrun_libs(&config)?;
                let b = LibkrunBackend::new(*config, libs)?;
                libkrun = Some(b.clone());
                Arc::new(b)
            }
            BackendConfig::Custom { backend } => backend,
        };

        if self.require_hardware_isolation && !backend.capabilities().hardware_isolated {
            return Err(SandboxError::HardwareIsolationRequired(format!(
                "backend `{}` does not provide it",
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
                ready_timeout: self.ready_timeout.unwrap_or(defaults.ready_timeout),
                call_timeout: self.call_timeout.unwrap_or(defaults.call_timeout),
                template_pins: self.template_pins_dir.map(PinStore::open),
                mount_policy: self.mount_policy,
                ..defaults
            },
        )?;
        Ok(Sandcrab {
            manager,
            require_hardware_isolation: self.require_hardware_isolation,
            #[cfg(feature = "docker")]
            docker,
            #[cfg(all(feature = "firecracker", target_os = "linux"))]
            firecracker,
            #[cfg(all(
                feature = "libkrun",
                any(target_os = "linux", all(target_os = "macos", target_arch = "aarch64"))
            ))]
            libkrun,
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
    docker: Option<DockerBackend>,
    #[cfg(all(feature = "firecracker", target_os = "linux"))]
    firecracker: Option<FirecrackerBackend>,
    #[cfg(all(
        feature = "libkrun",
        any(target_os = "linux", all(target_os = "macos", target_arch = "aarch64"))
    ))]
    libkrun: Option<LibkrunBackend>,
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

    /// Live sandboxes that match `filter`, oldest first: every given label must match
    /// exactly; `created_after` is inclusive, `created_before` exclusive.
    #[must_use]
    pub fn list(&self, filter: &SandboxFilter) -> Vec<SandboxInfo> {
        self.manager.list(filter)
    }

    /// A handle to the live sandbox `id`, e.g. one found with [`Self::list`]. Unlike the
    /// handle [`Self::spawn`] returns, dropping this one does not destroy the sandbox.
    pub fn sandbox(&self, id: SandboxId) -> Result<Sandbox> {
        Ok(Sandbox::borrowed(self.manager.get(id)?))
    }

    /// Destroy every sandbox that carries all of `labels` (at least one), which stops
    /// every program in them, then look again. [`DestroyByLabel::empty`] is true only when
    /// every destroy succeeded and nothing matches any more.
    pub async fn destroy_by_label(
        &self,
        labels: &BTreeMap<String, String>,
    ) -> Result<DestroyByLabel> {
        self.manager.destroy_by_label(labels).await
    }

    /// Change the TTL of sandbox `id` ([`Sandbox::set_ttl`]).
    pub fn set_ttl(&self, id: SandboxId, ttl: Option<Duration>) -> Result<()> {
        self.manager.set_ttl(id, ttl)
    }

    /// Destroy every live sandbox (returns the first error, after trying all).
    pub async fn shutdown(&self) -> Result<()> {
        self.manager.shutdown().await
    }

    /// The Docker backend, when that is the selected backend.
    #[cfg(feature = "docker")]
    #[must_use]
    pub fn docker_backend(&self) -> Option<&DockerBackend> {
        self.docker.as_ref()
    }

    /// The Firecracker backend, when that is the selected backend (selected with
    /// [`SandcrabBuilder::firecracker`]).
    #[cfg(all(feature = "firecracker", target_os = "linux"))]
    #[must_use]
    pub fn firecracker_backend(&self) -> Option<&FirecrackerBackend> {
        self.firecracker.as_ref()
    }

    /// The libkrun backend, when that is the selected backend (selected with
    /// [`SandcrabBuilder::libkrun`]).
    #[cfg(all(
        feature = "libkrun",
        any(target_os = "linux", all(target_os = "macos", target_arch = "aarch64"))
    ))]
    #[must_use]
    pub fn libkrun_backend(&self) -> Option<&LibkrunBackend> {
        self.libkrun.as_ref()
    }

    /// Check the template image present on the backend against its host-local pin
    /// (`sandcrab_core::pins`; local integrity, not provenance). Docker only for now.
    #[cfg(feature = "docker")]
    pub async fn verify_template(&self, name: &str) -> Result<PinStatus> {
        let template = self.manager.templates().get(name)?;
        let docker = self.docker.as_ref().ok_or_else(|| {
            SandboxError::InvalidConfig("template verification needs the docker backend".into())
        })?;
        let pin = sandcrab_core::effective_pin(
            self.manager.pin_store().as_ref(),
            &template.manifest,
            sandcrab_core::ImageArch::host(),
        )?;
        sandcrab_core::verify_image_pin(
            &docker.image_verifier(),
            template.image(),
            pin.as_ref().map(|p| p.digest.as_str()),
        )
        .await
    }

    /// Remove Sandcrab-managed Docker containers that this instance does not track
    /// (crash / cancelled-spawn leftovers). [`ReapScope::Owned`] only removes containers of
    /// this uid and state dir whose spawning process is gone; [`ReapScope::All`] is
    /// **host-wide** and also removes live sandboxes of other processes and users.
    #[cfg(feature = "docker")]
    pub async fn reap_orphans(&self, scope: ReapScope) -> Result<ReapReport> {
        let docker = self.docker.as_ref().ok_or_else(|| {
            SandboxError::InvalidConfig("orphan reaping needs the docker backend".into())
        })?;
        let tracked = self
            .manager
            .list_active()
            .into_iter()
            .map(|i| i.id)
            .collect();
        docker.reap_orphans(&tracked, scope).await
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
            fs::write(d.join("Dockerfile"), "FROM scratch\n").unwrap();
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
    async fn shared_kernel_backend_needs_no_opt_in_but_never_passes_hw_gate() {
        let env = Env::new();
        let sc = env
            .builder()
            .backend(FakeBackend::shared_kernel())
            .build()
            .unwrap();
        assert_eq!(sc.backend_kind(), BackendKind::Docker);
        let caps = sc.backend_capabilities();
        assert!(
            !caps.hardware_isolated && !caps.multi_tenant_safe,
            "{caps:?}"
        );

        let err = env
            .builder()
            .backend(FakeBackend::shared_kernel())
            .require_hardware_isolation(true)
            .build()
            .unwrap_err();
        assert!(
            matches!(err, SandboxError::HardwareIsolationRequired(_)),
            "{err}"
        );
    }

    /// The old opt-in is a deprecated no-op: it neither enables nor bypasses anything.
    #[tokio::test]
    #[allow(deprecated)]
    async fn deprecated_allow_insecure_backend_is_a_no_op() {
        let env = Env::new();
        for allow in [true, false] {
            let err = env
                .builder()
                .backend(FakeBackend::shared_kernel())
                .allow_insecure_backend(allow)
                .require_hardware_isolation(true)
                .build()
                .unwrap_err();
            assert!(
                matches!(err, SandboxError::HardwareIsolationRequired(_)),
                "{err}"
            );
            env.builder()
                .backend(FakeBackend::shared_kernel())
                .allow_insecure_backend(allow)
                .build()
                .unwrap();
        }
    }

    /// No backend → error; a shared-kernel backend is never picked for you.
    #[cfg(feature = "docker")]
    #[tokio::test]
    async fn docker_is_never_an_implicit_default() {
        let env = Env::new();
        let err = env
            .builder()
            .require_hardware_isolation(true)
            .build()
            .unwrap_err();
        assert!(matches!(err, SandboxError::InvalidConfig(_)), "{err}");
    }

    /// `require_hardware_isolation` passes with a real `FirecrackerBackend` (constructed
    /// without host checks, so no KVM is needed) and its capabilities are reported as-is.
    #[cfg(all(feature = "firecracker", target_os = "linux"))]
    #[tokio::test]
    async fn firecracker_passes_hardware_isolation_gate() {
        let env = Env::new();
        let mut config =
            FirecrackerConfig::with_home(&env.tmp.path().join("fc"), "/nonexistent/crabd");
        config.mkfs = Some(firecracker::rootfs::MkfsStrategy::Fakeroot);
        let backend = FirecrackerBackend::new_unchecked(config).unwrap();
        let sc = env
            .builder()
            .backend(backend)
            .require_hardware_isolation(true)
            .build()
            .unwrap();
        assert_eq!(sc.backend_kind(), BackendKind::Firecracker);
        let caps = sc.backend_capabilities();
        assert!(caps.hardware_isolated && !caps.multi_tenant_safe);
    }

    /// Firecracker does not serve forwards: asking for one is refused with an error that
    /// says so, before anything is started.
    #[cfg(all(feature = "firecracker", target_os = "linux"))]
    #[tokio::test]
    async fn firecracker_refuses_a_forward_explicitly() {
        let env = Env::new();
        let mut config =
            FirecrackerConfig::with_home(&env.tmp.path().join("fc"), "/nonexistent/crabd");
        config.mkfs = Some(firecracker::rootfs::MkfsStrategy::Fakeroot);
        let backend = FirecrackerBackend::new_unchecked(config).unwrap();
        let sc = env.builder().backend(backend).build().unwrap();
        assert!(!sc.backend_capabilities().supports_forwards);
        let err = sc
            .spawn(SandboxConfig::from_template("base").forward(5432))
            .await
            .unwrap_err();
        assert!(matches!(err, SandboxError::PolicyUnenforceable(_)), "{err}");
        let msg = err.to_string();
        assert!(
            msg.contains("forwards") && msg.contains("firecracker"),
            "{msg}"
        );
        assert!(sc.list_sandboxes().is_empty());
    }

    /// `.firecracker(..)` is an explicit selection: without a usable `/dev/kvm` it is an
    /// error naming the device, never a silent switch to another backend.
    #[cfg(all(feature = "firecracker", target_os = "linux"))]
    #[tokio::test]
    async fn firecracker_without_kvm_is_an_error_not_a_fallback() {
        if std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(firecracker::KVM_DEVICE)
            .is_ok()
        {
            eprintln!("skipped: /dev/kvm is usable on this host");
            return;
        }
        let env = Env::new();
        let config = FirecrackerConfig::with_home(&env.tmp.path().join("fc"), "/x/crabd");
        let err = env
            .builder()
            .firecracker(config)
            .require_hardware_isolation(true)
            .build()
            .unwrap_err();
        assert!(matches!(err, SandboxError::Backend { .. }), "{err}");
        assert!(err.to_string().contains("/dev/kvm"), "{err}");
    }

    /// `require_hardware_isolation` passes with a real `LibkrunBackend` (constructed without
    /// host checks, so no libkrun, helper or hypervisor is needed) and its capabilities are
    /// reported as-is (`hardware_isolated`, not `multi_tenant_safe`).
    #[cfg(all(
        feature = "libkrun",
        any(target_os = "linux", all(target_os = "macos", target_arch = "aarch64"))
    ))]
    #[tokio::test]
    async fn libkrun_passes_hardware_isolation_gate() {
        let env = Env::new();
        let home = env.tmp.path().join("lk");
        let mut config = LibkrunConfig::with_home(&home, "/nonexistent/crabd");
        // Temp dirs can exceed the Unix socket path budget; nothing is created here.
        config.run_dir = PathBuf::from("/tmp/sandcrab-sdk-test-krun");
        let pinned = |name: &str| libkrun::helper_proto::PinnedFile {
            path: home.join(name),
            sha256: "0".repeat(64),
            stamp: None,
        };
        let libs = KrunLibs {
            libkrun: pinned("libkrun"),
            libkrunfw: pinned("libkrunfw"),
            netstack: None,
        };
        let backend = LibkrunBackend::new_unchecked(config, libs).unwrap();
        let sc = env
            .builder()
            .backend(backend)
            .require_hardware_isolation(true)
            .build()
            .unwrap();
        assert_eq!(sc.backend_kind(), BackendKind::Libkrun);
        let caps = sc.backend_capabilities();
        assert!(
            caps.hardware_isolated && !caps.multi_tenant_safe,
            "{caps:?}"
        );
        assert!(!caps.supports_snapshots && caps.supports_host_dir_mounts);
    }

    /// `.libkrun(..)` without installed artifacts is an error naming the install command,
    /// never a silent switch to another backend.
    #[cfg(all(
        feature = "libkrun",
        any(target_os = "linux", all(target_os = "macos", target_arch = "aarch64"))
    ))]
    #[tokio::test]
    async fn libkrun_without_artifacts_is_an_error_not_a_fallback() {
        let env = Env::new();
        let home = env.tmp.path().join("lk");
        let mut config = LibkrunConfig::with_home(&home, "/x/crabd");
        config.lib_dir = Some(home.join("artifacts-missing"));
        let err = env
            .builder()
            .libkrun(config)
            .require_hardware_isolation(true)
            .build()
            .unwrap_err();
        assert!(
            !matches!(err, SandboxError::HardwareIsolationRequired(_)),
            "{err}"
        );
        assert!(err.to_string().contains("libkrun"), "{err}");
    }

    #[cfg(feature = "docker")]
    #[tokio::test]
    async fn docker_fails_hardware_isolation_gate() {
        let env = Env::new();
        let err = env
            .builder()
            .docker(DockerConfig::default())
            .require_hardware_isolation(true)
            .build()
            .unwrap_err();
        assert!(
            matches!(err, SandboxError::HardwareIsolationRequired(_)),
            "{err}"
        );
        assert!(err.to_string().contains("docker"), "{err}");
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

    fn labels(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[tokio::test]
    async fn list_and_destroy_by_label() {
        let env = Env::new();
        let backend = FakeBackend::new();
        let sc = env.sandcrab(&backend);
        let spawn =
            |run: &'static str| sc.spawn(SandboxConfig::from_template("base").label("run", run));
        let a = spawn("r1").await.unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        let b = spawn("r1").await.unwrap();
        let c = spawn("r2").await.unwrap();
        assert_eq!(a.labels(), &labels(&[("run", "r1")]));

        let ids = |f: &SandboxFilter| sc.list(f).iter().map(|i| i.id).collect::<Vec<_>>();
        let r1 = SandboxFilter {
            labels: labels(&[("run", "r1")]),
            ..SandboxFilter::default()
        };
        assert_eq!(ids(&SandboxFilter::default()), [a.id(), b.id(), c.id()]);
        assert_eq!(ids(&r1), [a.id(), b.id()]);
        // Only what was created since a point in time.
        let since_b = SandboxFilter {
            created_after: Some(b.info().created_at),
            ..r1.clone()
        };
        assert_eq!(ids(&since_b), [b.id()]);

        // A handle found by id does not own the sandbox.
        let found = sc.sandbox(a.id()).unwrap();
        assert_eq!(found.bash("hi").await.unwrap().stdout, "hi");
        drop(found);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!a.is_destroyed());

        assert!(sc.destroy_by_label(&BTreeMap::new()).await.is_err());
        let gone = sc
            .destroy_by_label(&labels(&[("run", "r1")]))
            .await
            .unwrap();
        assert_eq!((gone.matched, gone.destroyed), (2, 2));
        assert!(gone.empty && gone.remaining.is_empty());
        assert!(a.is_destroyed() && b.is_destroyed() && !c.is_destroyed());
        assert_eq!(ids(&SandboxFilter::default()), [c.id()]);
        assert!(matches!(
            sc.sandbox(a.id()),
            Err(SandboxError::NotFound { .. })
        ));
        let again = sc
            .destroy_by_label(&labels(&[("run", "r1")]))
            .await
            .unwrap();
        assert!(again.empty && again.matched == 0);
        c.destroy().await.unwrap();
    }

    #[tokio::test]
    async fn ttl_can_be_renewed_and_removed() {
        let env = Env::new();
        let backend = FakeBackend::new();
        let sc = env.sandcrab(&backend);
        let sb = sc
            .spawn(SandboxConfig::from_template("base").ttl(Some(Duration::from_secs(60))))
            .await
            .unwrap();
        assert!(sb.info().expires_in.unwrap() <= Duration::from_secs(60));
        sb.set_ttl(Some(Duration::from_secs(3600))).unwrap();
        assert!(sb.info().expires_in.unwrap() > Duration::from_secs(3000));
        sc.set_ttl(sb.id(), None).unwrap();
        assert_eq!(sb.info().expires_in, None);
        assert!(matches!(
            sb.set_ttl(Some(Duration::ZERO)),
            Err(SandboxError::InvalidConfig(_))
        ));
        let id = sb.id();
        sb.destroy().await.unwrap();
        assert!(matches!(
            sc.set_ttl(id, Some(Duration::from_secs(1))),
            Err(SandboxError::NotFound { .. })
        ));
    }

    #[tokio::test]
    async fn host_mounts_need_the_mount_policy_and_options_reach_the_backend() {
        let env = Env::new();
        let backend = FakeBackend::shared_kernel();
        let root = std::fs::canonicalize(env.tmp.path()).unwrap().join("data");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        let config = || {
            SandboxConfig::from_template("base")
                .host_mount(root.join("sub"), MountMode::ReadWrite)
                .forward(5432)
                .label("run", "r1")
                .tmp_exec(true)
                .tmp_size_mb(256)
                .resources(ResourceOverrides {
                    pids_max: Some(128),
                    nofile: Some(4096),
                    ..ResourceOverrides::default()
                })
        };

        // No policy on the builder: every host mount is refused.
        let sc = env.sandcrab(&backend);
        let err = sc.spawn(config()).await.unwrap_err();
        assert!(matches!(err, SandboxError::MountNotAllowed { .. }), "{err}");
        drop(sc);

        let policy = MountPolicy::new(
            vec![MountRoot {
                path: root.clone(),
                max_mode: MountMode::ReadWrite,
            }],
            Vec::new(),
        )
        .unwrap();
        let sc = env
            .builder()
            .backend(backend.clone())
            .mount_policy(policy)
            .build()
            .unwrap();
        // Outside the root: refused.
        let outside = SandboxConfig::from_template("base")
            .host_mount(root.parent().unwrap(), MountMode::ReadOnly);
        let err = sc.spawn(outside).await.unwrap_err();
        assert!(matches!(err, SandboxError::MountNotAllowed { .. }), "{err}");

        let sb = sc.spawn(config()).await.unwrap();
        let spec = backend.last_spec().unwrap();
        assert_eq!(spec.host_mounts.len(), 1);
        assert_eq!(spec.host_mounts[0].host_path, root.join("sub"));
        assert_eq!(spec.host_mounts[0].guest_path, root.join("sub"));
        assert_eq!(spec.forwards, [ForwardSpec::new(5432)]);
        assert!(spec.tmp_exec);
        assert_eq!(spec.tmp_size_mb, Some(256));
        assert_eq!((spec.pids_max, spec.nofile), (Some(128), Some(4096)));
        let info = sb.info();
        assert_eq!(
            info.host_mounts,
            [HostMount::new(root.join("sub"), MountMode::ReadWrite)]
        );
        assert_eq!(info.labels, labels(&[("run", "r1")]));

        // The sandbox's view of the mount is the host's directory (the fake answers from
        // the host filesystem, as a bind mount would).
        let sub = root.join("sub");
        let ids = sb.fs_identity([sub.to_str().unwrap()]).await.unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let meta = std::fs::metadata(&sub).unwrap();
            assert_eq!((ids[0].dev, ids[0].ino), (meta.dev(), meta.ino()));
        }
        assert_eq!(ids.len(), 1);
        assert!(sb.fs_identity(["/no/such/dir"]).await.is_err());
        sb.destroy().await.unwrap();
        drop(sc);

        // A backend that cannot do one of the options says so; nothing is dropped silently.
        let mut plain = FakeBackend::new();
        plain.caps.supports_process_limits = false;
        let sc = env.sandcrab(&plain);
        assert!(!sc.backend_capabilities().supports_process_limits);
        let limits = SandboxConfig::from_template("base").resources(ResourceOverrides {
            pids_max: Some(128),
            ..ResourceOverrides::default()
        });
        let err = sc.spawn(limits).await.unwrap_err();
        assert!(matches!(err, SandboxError::PolicyUnenforceable(_)), "{err}");
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
