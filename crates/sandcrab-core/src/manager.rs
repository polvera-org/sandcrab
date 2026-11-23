//! `SandboxManager`: policy gate, resolution, readiness, live-sandbox tracking, TTL reaper.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use sandcrab_protocol::{HelloResult, ProtocolError, ToolCall, ToolResult};
use serde::Serialize;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::client::DEFAULT_CALL_TIMEOUT;
use crate::{
    BackendCapabilities, BackendKind, CrabdClient, LifecyclePhase, LifecycleTimings, NetworkPolicy,
    PublishedPort, ResolvedSpec, Result, RuntimeBackend, SandboxConfig, SandboxError,
    SandboxHandle, SandboxId, TemplateRegistry, TimingMarks, WorkspaceLease, WorkspaceRegistry,
    resolve_mounts, wait_ready,
};

/// Manager-wide policy and timeouts.
#[derive(Debug, Clone)]
pub struct ManagerConfig {
    /// Allow a backend without hardware isolation (DockerDev). Must be set explicitly;
    /// there is never a silent fallback to an insecure backend.
    pub allow_insecure_backend: bool,
    /// Deadline for connect + `hello` after `RuntimeBackend::spawn` returns.
    pub ready_timeout: Duration,
    /// How often the TTL reaper scans for expired sandboxes.
    pub reaper_interval: Duration,
    /// Default per-call timeout for tool calls.
    pub call_timeout: Duration,
    /// How long [`SandboxManager::shutdown`] waits for all teardowns (including ones
    /// already in flight) before reporting the stragglers as failed.
    pub shutdown_timeout: Duration,
}

impl Default for ManagerConfig {
    fn default() -> Self {
        Self {
            allow_insecure_backend: false,
            ready_timeout: Duration::from_secs(30),
            reaper_interval: Duration::from_secs(1),
            call_timeout: DEFAULT_CALL_TIMEOUT,
            shutdown_timeout: Duration::from_secs(60),
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

struct ManagerInner {
    backend: Arc<dyn RuntimeBackend>,
    workspaces: WorkspaceRegistry,
    templates: TemplateRegistry,
    config: ManagerConfig,
    active: Mutex<HashMap<SandboxId, Sandbox>>,
    reaper: Mutex<Option<JoinHandle<()>>>,
}

impl Drop for ManagerInner {
    fn drop(&mut self) {
        if let Some(reaper) = lock(&self.reaper).take() {
            reaper.abort();
        }
        // Best-effort teardown of everything still running, even if callers still hold
        // `Sandbox` clones: the manager owns sandbox lifetimes.
        let all: Vec<Sandbox> = lock(&self.active).drain().map(|(_, s)| s).collect();
        for sandbox in all {
            sandbox.shared.start_teardown();
        }
    }
}

/// Creates, tracks and destroys sandboxes on one backend. Cheap to clone.
#[derive(Clone)]
pub struct SandboxManager {
    inner: Arc<ManagerInner>,
}

impl fmt::Debug for SandboxManager {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SandboxManager")
            .field("backend", &self.inner.backend.kind())
            .field("active", &lock(&self.inner.active).len())
            .finish_non_exhaustive()
    }
}

impl SandboxManager {
    /// Fails with [`SandboxError::InsecureBackendRejected`] if the backend is not hardware
    /// isolated and `config.allow_insecure_backend` is false.
    pub fn new(
        backend: Arc<dyn RuntimeBackend>,
        workspaces: WorkspaceRegistry,
        templates: TemplateRegistry,
        config: ManagerConfig,
    ) -> Result<Self> {
        let caps = backend.capabilities();
        if !caps.hardware_isolated && !config.allow_insecure_backend {
            return Err(SandboxError::InsecureBackendRejected(format!(
                "backend `{}` is not hardware isolated; set allow_insecure_backend to use it \
                 (local dev/CI only)",
                backend.kind()
            )));
        }
        Ok(Self {
            inner: Arc::new(ManagerInner {
                backend,
                workspaces,
                templates,
                config,
                active: Mutex::new(HashMap::new()),
                reaper: Mutex::new(None),
            }),
        })
    }

    #[must_use]
    pub fn backend_kind(&self) -> BackendKind {
        self.inner.backend.kind()
    }

    #[must_use]
    pub fn backend_capabilities(&self) -> BackendCapabilities {
        self.inner.backend.capabilities()
    }

    #[must_use]
    pub fn workspaces(&self) -> &WorkspaceRegistry {
        &self.inner.workspaces
    }

    #[must_use]
    pub fn templates(&self) -> &TemplateRegistry {
        &self.inner.templates
    }

    /// Validate `config`, enforce policy, resolve template + mounts, lease workspaces,
    /// spawn on the backend and wait for `crabd` readiness.
    pub async fn spawn(&self, config: SandboxConfig) -> Result<Sandbox> {
        let timings = LifecycleTimings::start();
        let inner = &self.inner;
        let backend = &inner.backend;
        config.validate()?;

        let caps = backend.capabilities();
        if config.require_hardware_isolation && !caps.hardware_isolated {
            return Err(SandboxError::InsecureBackendRejected(format!(
                "hardware isolation required but backend `{}` does not provide it",
                backend.kind()
            )));
        }
        if !config.secrets.is_empty() {
            return Err(SandboxError::PolicyUnenforceable(
                "secrets are reserved for Phase 3; no V1 backend can inject them".into(),
            ));
        }

        let template = inner.templates.get(&config.template)?.clone();
        let manifest = &template.manifest;
        let network = config
            .network
            .clone()
            .unwrap_or_else(|| manifest.network.to_config());
        network.validate()?;
        if !backend.enforces_network_policy(&network) {
            let hint = if network.policy == NetworkPolicy::EgressRestricted {
                " (no V1 backend enforces egress_restricted; use `disabled` or `unrestricted`)"
            } else {
                ""
            };
            return Err(SandboxError::PolicyUnenforceable(format!(
                "backend `{}` cannot enforce network policy `{}`{hint}",
                backend.kind(),
                network.policy
            )));
        }
        let resources = config.resources.apply(&manifest.resources)?;
        let user = config.user.or(manifest.template.user);

        let resolved = resolve_mounts(
            &config.mounts,
            config.default_cwd.as_deref(),
            &manifest.workspace.workdir,
            |r| inner.workspaces.get(r),
        )?;

        let id = SandboxId::new();
        let mut mounts = resolved.mounts;
        let mut leases = Vec::with_capacity(mounts.len());
        for m in &mut mounts {
            let lease = inner.workspaces.lease(&m.workspace_id.into(), m.mode, id)?;
            // The verified (symlink-free, dev/ino-checked) root; backends re-check it with
            // `verify_workspace_root` right before they use it.
            m.host_path = lease.root().to_path_buf();
            m.root_identity = lease.root_identity();
            m.generation = lease.generation();
            leases.push(lease);
        }

        let spec = ResolvedSpec {
            id,
            image: template.image().to_owned(),
            image_sha256: manifest.template.sha256.clone(),
            mounts,
            default_cwd: resolved.default_cwd.clone(),
            env: config.env.clone(),
            network,
            ports: config.ports.clone(),
            user,
            resources,
            ttl: config.ttl_duration(),
            secrets: Vec::new(),
            timings: timings.clone(),
            template,
        };

        let handle = backend.spawn(&spec).await?;
        timings.mark(LifecyclePhase::BackendStarted);
        // From here on, a cancelled spawn future must not leak the sandbox.
        let mut guard = SpawnGuard(Some(handle));
        let connector = guard.handle().connector();
        let ready = match wait_ready(
            connector.as_ref(),
            Instant::now() + inner.config.ready_timeout,
        )
        .await
        {
            Ok(r) => r,
            Err(e) => {
                if let Some(handle) = guard.0.take()
                    && let Err(de) = handle.destroy().await
                {
                    tracing::warn!(%id, error = %de, "destroy after failed readiness also failed");
                }
                return Err(e);
            }
        };
        timings.mark_at(LifecyclePhase::Ready, ready.ready_at);
        let handle = guard.0.take().expect("handle present until readiness");
        let ports = handle.published_ports();

        let now = Instant::now();
        let sandbox = Sandbox {
            shared: Arc::new(SandboxShared {
                id,
                template: config.template.clone(),
                backend: backend.kind(),
                client: ready.client.with_default_timeout(inner.config.call_timeout),
                hello: ready.hello,
                timings,
                ports,
                default_cwd: resolved.default_cwd,
                created_at: Utc::now(),
                expires_at: config.ttl_duration().map(|ttl| now + ttl),
                destroyed: AtomicBool::new(false),
                live: Mutex::new(Some(Live { handle, leases })),
                teardown: watch::Sender::new(None),
                manager: Arc::downgrade(&self.inner),
            }),
        };
        lock(&inner.active).insert(id, sandbox.clone());
        self.ensure_reaper();
        Ok(sandbox)
    }

    /// Live sandboxes, oldest first.
    #[must_use]
    pub fn list_active(&self) -> Vec<SandboxInfo> {
        let mut all: Vec<SandboxInfo> = lock(&self.inner.active)
            .values()
            .map(Sandbox::info)
            .collect();
        all.sort_by_key(|i| i.created_at);
        all
    }

    pub fn get(&self, id: SandboxId) -> Result<Sandbox> {
        lock(&self.inner.active)
            .get(&id)
            .cloned()
            .ok_or_else(|| SandboxError::not_found("sandbox", id))
    }

    /// Destroy a live sandbox; returns its final timing marks.
    pub async fn destroy(&self, id: SandboxId) -> Result<TimingMarks> {
        self.get(id)?.destroy().await
    }

    /// Destroy every tracked sandbox and wait for all teardowns — including ones already in
    /// flight (e.g. started by a dropped SDK handle) — up to
    /// [`ManagerConfig::shutdown_timeout`]. Returns an error naming every sandbox whose
    /// teardown failed or did not finish in time; never `Ok` while one is still running.
    pub async fn shutdown(&self) -> Result<()> {
        let all: Vec<Sandbox> = lock(&self.inner.active).values().cloned().collect();
        let timeout = self.inner.config.shutdown_timeout;
        let deadline = tokio::time::Instant::now() + timeout;
        let mut pending: HashSet<SandboxId> = all.iter().map(Sandbox::id).collect();
        let mut tasks = tokio::task::JoinSet::new();
        for s in all {
            tasks.spawn(async move { (s.id(), s.destroy().await) });
        }
        let mut failures = Vec::new();
        loop {
            match tokio::time::timeout_at(deadline, tasks.join_next()).await {
                Ok(None) => break,
                Ok(Some(Ok((id, result)))) => {
                    pending.remove(&id);
                    if let Err(e) = result {
                        tracing::warn!(%id, error = %e, "destroy during shutdown failed");
                        failures.push(format!("{id}: {e}"));
                    }
                }
                Ok(Some(Err(join))) => failures.push(format!("teardown waiter failed: {join}")),
                Err(_) => {
                    for id in &pending {
                        tracing::warn!(%id, ?timeout, "teardown still running at shutdown deadline");
                        failures.push(format!("{id}: teardown not finished after {timeout:?}"));
                    }
                    break;
                }
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(SandboxError::Backend {
                backend: self.inner.backend.kind().to_string(),
                message: format!(
                    "shutdown failed to tear down {} sandbox(es): {}",
                    failures.len(),
                    failures.join("; ")
                ),
            })
        }
    }

    /// Destroy sandboxes whose TTL has passed; returns how many were reaped.
    pub async fn reap_expired(&self) -> usize {
        reap(&self.inner).await
    }

    fn ensure_reaper(&self) {
        let mut slot = lock(&self.inner.reaper);
        if slot.as_ref().is_some_and(|h| !h.is_finished()) {
            return;
        }
        let weak = Arc::downgrade(&self.inner);
        let interval = self.inner.config.reaper_interval;
        *slot = Some(tokio::spawn(async move {
            let mut tick = tokio::time::interval(interval);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                let Some(inner) = weak.upgrade() else { break };
                let expired = expired(&inner);
                drop(inner);
                destroy_all(expired).await;
            }
        }));
    }
}

fn expired(inner: &ManagerInner) -> Vec<Sandbox> {
    let now = Instant::now();
    lock(&inner.active)
        .values()
        .filter(|s| s.shared.expires_at.is_some_and(|t| t <= now))
        .cloned()
        .collect()
}

async fn destroy_all(sandboxes: Vec<Sandbox>) -> usize {
    let n = sandboxes.len();
    for s in sandboxes {
        tracing::info!(id = %s.id(), "TTL expired; destroying sandbox");
        if let Err(e) = s.destroy().await {
            tracing::warn!(id = %s.id(), error = %e, "TTL destroy failed");
        }
    }
    n
}

async fn reap(inner: &ManagerInner) -> usize {
    destroy_all(expired(inner)).await
}

/// Destroys the handle in the background if `spawn` is cancelled before readiness.
struct SpawnGuard(Option<Box<dyn SandboxHandle>>);

impl SpawnGuard {
    fn handle(&self) -> &dyn SandboxHandle {
        self.0.as_deref().expect("handle present")
    }
}

impl Drop for SpawnGuard {
    fn drop(&mut self) {
        if let Some(handle) = self.0.take() {
            spawn_destroy(handle, Vec::new());
        }
    }
}

/// Fire-and-forget destroy; leases are released once the backend is done.
fn spawn_destroy(handle: Box<dyn SandboxHandle>, leases: Vec<WorkspaceLease>) {
    match tokio::runtime::Handle::try_current() {
        Ok(rt) => {
            rt.spawn(async move {
                let id = handle.id();
                if let Err(e) = handle.destroy().await {
                    tracing::warn!(%id, error = %e, "best-effort destroy failed");
                    leases.into_iter().for_each(release_after_failure);
                }
            });
        }
        Err(_) => {
            tracing::warn!(
                id = %handle.id(),
                "no tokio runtime; cannot destroy sandbox, dropping handle"
            );
            leases.into_iter().for_each(release_after_failure);
        }
    }
}

/// A rw workspace whose sandbox could not be torn down cleanly is flagged dirty.
fn release_after_failure(lease: WorkspaceLease) {
    if lease.mode().is_writable() {
        lease.release_failed();
    }
}

struct Live {
    handle: Box<dyn SandboxHandle>,
    leases: Vec<WorkspaceLease>,
}

/// Result of a sandbox teardown, shared by every caller awaiting it.
type TeardownOutcome = Result<TimingMarks>;

struct SandboxShared {
    id: SandboxId,
    template: String,
    backend: BackendKind,
    client: CrabdClient,
    hello: HelloResult,
    timings: LifecycleTimings,
    ports: Vec<PublishedPort>,
    default_cwd: PathBuf,
    created_at: DateTime<Utc>,
    expires_at: Option<Instant>,
    destroyed: AtomicBool,
    /// Backend handle + leases until the (single) teardown claims them.
    live: Mutex<Option<Live>>,
    /// `Some` once the teardown has finished, successfully or not.
    teardown: watch::Sender<Option<TeardownOutcome>>,
    manager: Weak<ManagerInner>,
}

impl SandboxShared {
    /// Single-flight teardown: the first caller starts it, every caller (explicit destroy,
    /// SDK drop guard, TTL reaper, `shutdown`) awaits the same outcome. The sandbox stays
    /// in the manager's active list until the backend teardown has finished.
    async fn destroy(self: &Arc<Self>) -> Result<TimingMarks> {
        self.start_teardown();
        let mut rx = self.teardown.subscribe();
        match rx.wait_for(Option::is_some).await {
            Ok(outcome) => outcome.clone().expect("checked by wait_for"),
            // Unreachable: the sender lives in `self`.
            Err(_) => Err(self.teardown_error("teardown outcome channel closed")),
        }
    }

    /// Claim the live handle and run its teardown on a runtime task (so a cancelled caller
    /// cannot abort it). No-op if a teardown was already started.
    fn start_teardown(self: &Arc<Self>) {
        let Some(Live { handle, leases }) = lock(&self.live).take() else {
            return;
        };
        self.destroyed.store(true, Ordering::SeqCst);
        self.timings.mark(LifecyclePhase::TeardownStarted);
        let mut guard = TeardownGuard {
            shared: Arc::clone(self),
            leases: Some(leases),
        };
        match tokio::runtime::Handle::try_current() {
            Ok(rt) => {
                rt.spawn(async move {
                    guard.shared.client.close().await;
                    let result = handle.destroy().await;
                    guard.complete(result);
                });
            }
            Err(_) => {
                tracing::warn!(
                    id = %self.id,
                    "no tokio runtime; cannot destroy sandbox, dropping handle"
                );
                drop(handle);
                guard.complete(Err(self.teardown_error("no tokio runtime to run teardown")));
            }
        }
    }

    fn teardown_error(&self, message: &str) -> SandboxError {
        SandboxError::Backend {
            backend: self.backend.to_string(),
            message: format!("sandbox {}: {message}", self.id),
        }
    }
}

/// Owns a running teardown's leases and guarantees an outcome is published even if the
/// task is cancelled (runtime shutdown) or panics, so waiters never hang.
struct TeardownGuard {
    shared: Arc<SandboxShared>,
    leases: Option<Vec<WorkspaceLease>>,
}

impl TeardownGuard {
    fn complete(&mut self, result: Result<()>) {
        let Some(leases) = self.leases.take() else {
            return;
        };
        match &result {
            Ok(()) => drop(leases),
            Err(e) => {
                tracing::warn!(id = %self.shared.id, error = %e, "sandbox teardown failed");
                leases.into_iter().for_each(release_after_failure);
            }
        }
        let shared = &self.shared;
        shared.timings.mark(LifecyclePhase::TeardownFinished);
        let outcome = result.map(|()| shared.timings.snapshot());
        // Leave the active list before waking waiters, so a returned destroy/shutdown
        // never observes the sandbox still listed.
        if let Some(manager) = shared.manager.upgrade() {
            let removed = lock(&manager.active).remove(&shared.id);
            drop(removed);
        }
        shared.teardown.send_replace(Some(outcome));
    }
}

impl Drop for TeardownGuard {
    fn drop(&mut self) {
        if self.leases.is_some() {
            let err = self
                .shared
                .teardown_error("teardown aborted before completion");
            self.complete(Err(err));
        }
    }
}

impl Drop for SandboxShared {
    fn drop(&mut self) {
        let live = self
            .live
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(Live { handle, leases }) = live {
            spawn_destroy(handle, leases);
        }
    }
}

/// Snapshot of a live sandbox for listings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SandboxInfo {
    pub id: SandboxId,
    pub template: String,
    pub backend: BackendKind,
    pub created_at: DateTime<Utc>,
    /// Time left before the TTL reaper destroys it.
    pub expires_in: Option<Duration>,
    pub default_cwd: PathBuf,
    pub published_ports: Vec<PublishedPort>,
}

/// A live sandbox: typed tool-call proxy over its `crabd` connection. Cheap to clone.
#[derive(Clone)]
pub struct Sandbox {
    shared: Arc<SandboxShared>,
}

impl fmt::Debug for Sandbox {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sandbox")
            .field("id", &self.shared.id)
            .field("template", &self.shared.template)
            .field("backend", &self.shared.backend)
            .field("destroyed", &self.is_destroyed())
            .finish_non_exhaustive()
    }
}

impl Sandbox {
    #[must_use]
    pub fn id(&self) -> SandboxId {
        self.shared.id
    }

    #[must_use]
    pub fn template(&self) -> &str {
        &self.shared.template
    }

    #[must_use]
    pub fn backend(&self) -> BackendKind {
        self.shared.backend
    }

    /// `crabd`'s `hello` reply (version, tools, default cwd).
    #[must_use]
    pub fn hello(&self) -> &HelloResult {
        &self.shared.hello
    }

    #[must_use]
    pub fn default_cwd(&self) -> &Path {
        &self.shared.default_cwd
    }

    #[must_use]
    pub fn published_ports(&self) -> &[PublishedPort] {
        &self.shared.ports
    }

    #[must_use]
    pub fn timings(&self) -> &LifecycleTimings {
        &self.shared.timings
    }

    #[must_use]
    pub fn expires_at(&self) -> Option<Instant> {
        self.shared.expires_at
    }

    #[must_use]
    pub fn is_destroyed(&self) -> bool {
        self.shared.destroyed.load(Ordering::SeqCst)
    }

    #[must_use]
    pub fn info(&self) -> SandboxInfo {
        let s = &self.shared;
        SandboxInfo {
            id: s.id,
            template: s.template.clone(),
            backend: s.backend,
            created_at: s.created_at,
            expires_in: s
                .expires_at
                .map(|t| t.saturating_duration_since(Instant::now())),
            default_cwd: s.default_cwd.clone(),
            published_ports: s.ports.clone(),
        }
    }

    /// Invoke a tool. The first response (success or tool error) marks `first_call`.
    pub async fn call_tool(&self, call: ToolCall) -> Result<ToolResult> {
        if self.is_destroyed() {
            return Err(SandboxError::not_found("sandbox", self.id()));
        }
        let result = self.shared.client.call(call).await;
        if matches!(result, Ok(_) | Err(SandboxError::Protocol(_))) {
            self.shared.timings.mark(LifecyclePhase::FirstCall);
        }
        result
    }

    /// Invoke a tool and extract its typed result, e.g.
    /// `sandbox.call::<BashResult>(BashParams { .. })`.
    pub async fn call<R>(&self, call: impl Into<ToolCall>) -> Result<R>
    where
        R: TryFrom<ToolResult, Error = ProtocolError>,
    {
        Ok(R::try_from(self.call_tool(call.into()).await?)?)
    }

    /// Tear down (idempotent). Returns the final timing marks.
    pub async fn destroy(&self) -> Result<TimingMarks> {
        self.shared.destroy().await
    }
}

#[cfg(test)]
mod tests {
    use sandcrab_protocol::{BashParams, BashResult};
    use std::fs;

    use super::*;
    use crate::testing::{FakeBackend, FakeEvent, bash};
    use crate::{
        MountMode, NetworkConfig, ResourceOverrides, SecretSource, SecretSpec, UserSpec,
        WorkspaceState,
    };

    struct Env {
        _tmp: tempfile::TempDir,
        workspaces: WorkspaceRegistry,
        templates: TemplateRegistry,
        project: PathBuf,
    }

    fn env() -> Env {
        let tmp = tempfile::tempdir().unwrap();
        let tdir = tmp.path().join("templates");
        for (name, extra) in [
            ("base", "user = \"1000:1000\"\n"),
            (
                "restricted",
                "[network]\ndefault_policy = \"egress_restricted\"\n",
            ),
        ] {
            let d = tdir.join(name);
            fs::create_dir_all(&d).unwrap();
            let (meta, rest) = if extra.starts_with('[') {
                ("", extra)
            } else {
                (extra, "")
            };
            fs::write(
                d.join("sandcrab.toml"),
                format!(
                    "[template]\nname = \"{name}\"\nversion = \"0.1.0\"\nimage = \"sandcrab/{name}:0.1.0\"\n{meta}\n[resources]\nvcpu = 1\nmemory_mb = 512\ndisk_mb = 2048\n{rest}"
                ),
            )
            .unwrap();
        }
        let templates = TemplateRegistry::load(&[tdir]).unwrap();
        let workspaces = WorkspaceRegistry::open(tmp.path().join("state")).unwrap();
        let project = tmp.path().join("project");
        fs::create_dir_all(&project).unwrap();
        workspaces.register_host_dir("proj", &project).unwrap();
        let project = fs::canonicalize(project).unwrap();
        Env {
            _tmp: tmp,
            workspaces,
            templates,
            project,
        }
    }

    fn manager(env: &Env, backend: &FakeBackend, config: ManagerConfig) -> SandboxManager {
        SandboxManager::new(
            Arc::new(backend.clone()),
            env.workspaces.clone(),
            env.templates.clone(),
            config,
        )
        .unwrap()
    }

    fn quick() -> ManagerConfig {
        ManagerConfig {
            ready_timeout: Duration::from_secs(2),
            reaper_interval: Duration::from_millis(50),
            ..ManagerConfig::default()
        }
    }

    #[tokio::test]
    async fn spawn_call_destroy() {
        let env = env();
        let backend = FakeBackend::new();
        let mgr = manager(&env, &backend, quick());
        let sb = mgr
            .spawn(
                SandboxConfig::from_template("base")
                    .mount("proj", "/workspace/", MountMode::ReadWrite)
                    .resources(ResourceOverrides {
                        vcpu: Some(3),
                        ..Default::default()
                    })
                    .port(8080),
            )
            .await
            .unwrap();

        // Resolved spec as seen by the backend.
        let spec = backend.last_spec().unwrap();
        assert_eq!(spec.id, sb.id());
        assert_eq!(spec.image, "sandcrab/base:0.1.0");
        assert_eq!(spec.mounts[0].host_path, env.project);
        assert_eq!(spec.mounts[0].guest_path, PathBuf::from("/workspace"));
        assert_eq!(spec.default_cwd, PathBuf::from("/workspace"));
        assert_eq!(spec.network, NetworkConfig::disabled());
        assert_eq!(spec.resources.vcpu, 3);
        assert_eq!(spec.resources.memory_mb, 512);
        assert_eq!(spec.user, Some(UserSpec::new(1000, 1000)));
        assert_eq!(spec.ports.len(), 1);
        assert_eq!(sb.published_ports().len(), 1);

        assert_eq!(
            env.workspaces.get(&"proj".into()).unwrap().state,
            WorkspaceState::LeasedRw { sandbox: sb.id() }
        );
        assert_eq!(mgr.list_active().len(), 1);

        let r: BashResult = sb
            .call(BashParams {
                command: "echo hi".into(),
                cwd: None,
                env: Default::default(),
                timeout_ms: None,
            })
            .await
            .unwrap();
        assert_eq!(r.stdout, "echo hi");

        let gen_before = env.workspaces.get(&"proj".into()).unwrap().generation;
        let marks = mgr.destroy(sb.id()).await.unwrap();
        assert!(marks.spawn_ready().is_some());
        assert!(marks.first_call().is_some());
        assert!(marks.teardown().is_some());
        assert!(marks.backend_started.is_some());

        assert!(sb.is_destroyed());
        assert_eq!(
            backend.events(),
            [FakeEvent::Spawned(sb.id()), FakeEvent::Destroyed(sb.id())]
        );
        assert!(mgr.list_active().is_empty());
        let ws = env.workspaces.get(&"proj".into()).unwrap();
        assert_eq!(ws.state, WorkspaceState::Idle);
        assert_eq!(ws.generation, gen_before + 1);
        assert!(matches!(
            sb.call_tool(bash("x")).await,
            Err(SandboxError::NotFound { .. })
        ));
        // Idempotent.
        sb.destroy().await.unwrap();
        assert!(matches!(
            mgr.destroy(sb.id()).await,
            Err(SandboxError::NotFound { .. })
        ));
    }

    #[tokio::test]
    async fn insecure_backend_needs_explicit_opt_in() {
        let env = env();
        let backend = FakeBackend::insecure();
        assert!(matches!(
            SandboxManager::new(
                Arc::new(backend.clone()),
                env.workspaces.clone(),
                env.templates.clone(),
                ManagerConfig::default()
            ),
            Err(SandboxError::InsecureBackendRejected(_))
        ));
        let mgr = manager(
            &env,
            &backend,
            ManagerConfig {
                allow_insecure_backend: true,
                ..quick()
            },
        );
        mgr.spawn(SandboxConfig::from_template("base"))
            .await
            .unwrap();
        let err = mgr
            .spawn(SandboxConfig::from_template("base").require_hardware_isolation(true))
            .await
            .unwrap_err();
        assert!(
            matches!(err, SandboxError::InsecureBackendRejected(_)),
            "{err:?}"
        );
        assert_eq!(backend.events().len(), 1, "rejected before backend.spawn");
    }

    #[tokio::test]
    async fn network_policy_gate() {
        let env = env();
        let backend = FakeBackend::new(); // enforces disabled only
        let mgr = manager(&env, &backend, quick());

        // Template default egress_restricted → refused, never silently downgraded.
        let err = mgr
            .spawn(SandboxConfig::from_template("restricted"))
            .await
            .unwrap_err();
        assert!(
            matches!(err, SandboxError::PolicyUnenforceable(_)),
            "{err:?}"
        );
        let err = mgr
            .spawn(SandboxConfig::from_template("base").network(NetworkConfig::unrestricted()))
            .await
            .unwrap_err();
        assert!(
            matches!(err, SandboxError::PolicyUnenforceable(_)),
            "{err:?}"
        );
        assert!(backend.events().is_empty());

        // Explicit config overrides the template default.
        mgr.spawn(SandboxConfig::from_template("restricted").network(NetworkConfig::disabled()))
            .await
            .unwrap();
        assert_eq!(
            backend.last_spec().unwrap().network.policy,
            NetworkPolicy::Disabled
        );
    }

    #[tokio::test]
    async fn secrets_are_rejected() {
        let env = env();
        let backend = FakeBackend::new();
        let mgr = manager(&env, &backend, quick());
        let mut cfg = SandboxConfig::from_template("base");
        cfg.secrets.push(SecretSpec {
            name: "TOKEN".into(),
            source: SecretSource::HostEnv { var: "T".into() },
        });
        assert!(matches!(
            mgr.spawn(cfg).await,
            Err(SandboxError::PolicyUnenforceable(_))
        ));
        assert!(backend.events().is_empty());
    }

    #[tokio::test]
    async fn invalid_requests_fail_before_backend() {
        let env = env();
        let backend = FakeBackend::new();
        let mgr = manager(&env, &backend, quick());
        for (cfg, what) in [
            (SandboxConfig::from_template("nope"), "unknown template"),
            (
                SandboxConfig::from_template("base").mount("ghost", "/w", MountMode::ReadWrite),
                "unknown workspace",
            ),
            (
                SandboxConfig::from_template("base").mount("proj", "/proc/x", MountMode::ReadOnly),
                "forbidden target",
            ),
            (
                SandboxConfig::from_template("base").resources(ResourceOverrides {
                    vcpu: Some(0),
                    ..Default::default()
                }),
                "bad resources",
            ),
        ] {
            assert!(mgr.spawn(cfg).await.is_err(), "{what}");
        }
        assert!(backend.events().is_empty());
        assert_eq!(
            env.workspaces.get(&"proj".into()).unwrap().state,
            WorkspaceState::Idle
        );
    }

    #[tokio::test]
    async fn rw_workspace_is_exclusive() {
        let env = env();
        let backend = FakeBackend::new();
        let mgr = manager(&env, &backend, quick());
        let cfg = SandboxConfig::from_template("base").mount("proj", "/w", MountMode::ReadWrite);
        let first = mgr.spawn(cfg.clone()).await.unwrap();
        assert!(matches!(
            mgr.spawn(cfg.clone()).await,
            Err(SandboxError::WorkspaceBusy(_))
        ));
        assert!(matches!(
            env.workspaces.put_file(&"proj".into(), "f", b"x"),
            Err(SandboxError::WorkspaceBusy(_))
        ));
        first.destroy().await.unwrap();
        mgr.spawn(cfg).await.unwrap();
    }

    #[tokio::test]
    async fn readiness_failure_cleans_up() {
        let env = env();
        let backend = FakeBackend::new();
        backend.connect_failures.store(u32::MAX, Ordering::SeqCst);
        let mgr = manager(
            &env,
            &backend,
            ManagerConfig {
                ready_timeout: Duration::from_millis(100),
                ..quick()
            },
        );
        let err = mgr
            .spawn(SandboxConfig::from_template("base").mount("proj", "/w", MountMode::ReadWrite))
            .await
            .unwrap_err();
        assert!(matches!(err, SandboxError::Timeout { .. }), "{err:?}");
        let events = backend.events();
        assert!(
            matches!(events.as_slice(), [FakeEvent::Spawned(a), FakeEvent::Destroyed(b)] if a == b)
        );
        assert_eq!(
            env.workspaces.get(&"proj".into()).unwrap().state,
            WorkspaceState::Idle
        );
        assert!(mgr.list_active().is_empty());
    }

    #[tokio::test]
    async fn backend_destroy_failure_marks_rw_workspace_dirty() {
        let env = env();
        let backend = FakeBackend::new();
        let mgr = manager(&env, &backend, quick());
        let sb = mgr
            .spawn(SandboxConfig::from_template("base").mount("proj", "/w", MountMode::ReadWrite))
            .await
            .unwrap();
        backend.fail_destroy.store(true, Ordering::SeqCst);
        assert!(sb.destroy().await.is_err());
        assert_eq!(
            env.workspaces.get(&"proj".into()).unwrap().state,
            WorkspaceState::DirtySession
        );
    }

    #[tokio::test]
    async fn ttl_reaper_destroys_expired() {
        let env = env();
        let backend = FakeBackend::new();
        let mgr = manager(&env, &backend, quick());
        let short = mgr
            .spawn(SandboxConfig::from_template("base").ttl(Some(Duration::from_secs(1))))
            .await
            .unwrap();
        let forever = mgr
            .spawn(SandboxConfig::from_template("base").ttl(None))
            .await
            .unwrap();
        assert_eq!(mgr.reap_expired().await, 0);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !short.is_destroyed() {
            assert!(Instant::now() < deadline, "reaper did not fire");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(!forever.is_destroyed());
        assert_eq!(
            mgr.list_active().iter().map(|i| i.id).collect::<Vec<_>>(),
            [forever.id()]
        );
    }

    #[tokio::test]
    async fn dropping_manager_destroys_sandboxes() {
        let env = env();
        let backend = FakeBackend::new();
        let mgr = manager(&env, &backend, quick());
        let sb = mgr
            .spawn(SandboxConfig::from_template("base"))
            .await
            .unwrap();
        drop(mgr);
        let deadline = Instant::now() + Duration::from_secs(2);
        while !backend.events().contains(&FakeEvent::Destroyed(sb.id())) {
            assert!(Instant::now() < deadline, "not destroyed on drop");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(sb.is_destroyed());
    }

    #[tokio::test]
    async fn shutdown_destroys_all() {
        let env = env();
        let backend = FakeBackend::new();
        let mgr = manager(&env, &backend, quick());
        for _ in 0..3 {
            mgr.spawn(SandboxConfig::from_template("base"))
                .await
                .unwrap();
        }
        mgr.shutdown().await.unwrap();
        assert!(mgr.list_active().is_empty());
        assert_eq!(
            backend
                .events()
                .iter()
                .filter(|e| matches!(e, FakeEvent::Destroyed(_)))
                .count(),
            3
        );
    }

    fn destroyed_count(backend: &FakeBackend) -> usize {
        backend
            .events()
            .iter()
            .filter(|e| matches!(e, FakeEvent::Destroyed(_)))
            .count()
    }

    #[tokio::test]
    async fn concurrent_destroys_are_single_flight() {
        let env = env();
        let backend = FakeBackend::new();
        backend.destroy_delay_ms.store(300, Ordering::SeqCst);
        let mgr = manager(&env, &backend, quick());
        let sb = mgr
            .spawn(SandboxConfig::from_template("base"))
            .await
            .unwrap();
        let id = sb.id();

        let first = tokio::spawn({
            let sb = sb.clone();
            async move { sb.destroy().await }
        });
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(sb.is_destroyed(), "calls are rejected once teardown starts");
        assert_eq!(mgr.list_active().len(), 1, "listed until teardown finishes");

        // Second caller joins the in-flight teardown instead of returning early.
        let started = Instant::now();
        mgr.destroy(id).await.unwrap();
        assert!(started.elapsed() >= Duration::from_millis(200));
        assert_eq!(backend.events().last(), Some(&FakeEvent::Destroyed(id)));
        assert!(mgr.list_active().is_empty());
        first.await.unwrap().unwrap();
        sb.destroy().await.unwrap();
        assert_eq!(destroyed_count(&backend), 1, "backend destroy ran once");
    }

    #[tokio::test]
    async fn concurrent_destroys_share_failure() {
        let env = env();
        let backend = FakeBackend::new();
        backend.destroy_delay_ms.store(100, Ordering::SeqCst);
        backend.fail_destroy.store(true, Ordering::SeqCst);
        let mgr = manager(&env, &backend, quick());
        let sb = mgr
            .spawn(SandboxConfig::from_template("base"))
            .await
            .unwrap();
        let (a, b) = tokio::join!(sb.destroy(), sb.destroy());
        for r in [a, b] {
            let err = r.unwrap_err();
            assert!(
                err.to_string().contains("injected destroy failure"),
                "{err}"
            );
        }
        assert!(mgr.list_active().is_empty(), "failed teardown is recorded");
    }

    #[tokio::test]
    async fn shutdown_awaits_in_flight_detached_destroy() {
        let env = env();
        let backend = FakeBackend::new();
        backend.destroy_delay_ms.store(300, Ordering::SeqCst);
        let mgr = manager(&env, &backend, quick());
        let sb = mgr
            .spawn(SandboxConfig::from_template("base"))
            .await
            .unwrap();
        let id = sb.id();
        // What the SDK drop guard does: fire-and-forget destroy on the runtime.
        drop(tokio::spawn(async move { sb.destroy().await }));
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(mgr.list_active().len(), 1);

        mgr.shutdown().await.unwrap();
        assert!(backend.events().contains(&FakeEvent::Destroyed(id)));
        assert!(mgr.list_active().is_empty());
        assert_eq!(destroyed_count(&backend), 1);
    }

    #[tokio::test]
    async fn shutdown_reports_teardowns_past_deadline() {
        let env = env();
        let backend = FakeBackend::new();
        backend.destroy_delay_ms.store(1_000, Ordering::SeqCst);
        let mgr = manager(
            &env,
            &backend,
            ManagerConfig {
                shutdown_timeout: Duration::from_millis(100),
                ..quick()
            },
        );
        let sb = mgr
            .spawn(SandboxConfig::from_template("base"))
            .await
            .unwrap();
        let err = mgr.shutdown().await.unwrap_err();
        assert!(err.to_string().contains(&sb.id().to_string()), "{err}");
        // The teardown itself keeps running and completes.
        sb.destroy().await.unwrap();
        assert!(mgr.list_active().is_empty());
    }

    #[tokio::test]
    async fn concurrent_calls_on_one_sandbox() {
        let env = env();
        let backend = FakeBackend::new();
        let mgr = manager(&env, &backend, quick());
        let sb = mgr
            .spawn(SandboxConfig::from_template("base"))
            .await
            .unwrap();
        let calls = (0..16).map(|i| {
            let sb = sb.clone();
            async move {
                sb.call::<BashResult>(bash(&format!("sleep:{}:{i}", 50 - i * 3)))
                    .await
            }
        });
        let results = futures_join_all(calls).await;
        for (i, r) in results.into_iter().enumerate() {
            assert!(r.unwrap().stdout.ends_with(&format!(":{i}")));
        }
    }

    async fn futures_join_all<F: std::future::Future + Send + 'static>(
        futs: impl Iterator<Item = F>,
    ) -> Vec<F::Output>
    where
        F::Output: Send + 'static,
    {
        let handles: Vec<_> = futs.map(tokio::spawn).collect();
        let mut out = Vec::new();
        for h in handles {
            out.push(h.await.unwrap());
        }
        out
    }
}
