//! `SandboxManager`: policy gate, resolution, readiness, live-sandbox tracking, TTL reaper.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use sandcrab_protocol::{
    FileIdentity, ForwardPortStats, HelloResult, ProcessCancelResult, ProcessOpenParams,
    ProtocolError, ToolCall, ToolResult, features,
};
use serde::Serialize;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::client::{Connector, DEFAULT_CALL_TIMEOUT};
use crate::forward::{ForwardAcceptor, ForwardConfig, ForwardTarget};
use crate::pins::{PinStore, effective_pin};
use crate::process::{DEFAULT_PROCESS_OPEN_TIMEOUT, ProcessStream, ProcessTable};
use crate::template::ImageArch;
use crate::{
    BackendCapabilities, BackendKind, CrabdClient, ForwardSpec, HostMount, LifecyclePhase,
    LifecycleTimings, MountPolicy, NetworkPolicy, PublishedPort, RequestedOptions, ResolvedSpec,
    Result, RuntimeBackend, SandboxConfig, SandboxError, SandboxHandle, SandboxId,
    TemplateRegistry, TimingMarks, WorkspaceLease, WorkspaceRegistry, resolve_mounts,
    validate_labels, wait_ready,
};

/// Manager-wide policy and timeouts.
#[derive(Debug, Clone)]
pub struct ManagerConfig {
    /// Deadline for connect + `hello` after `RuntimeBackend::spawn` returns.
    pub ready_timeout: Duration,
    /// How often the TTL reaper scans for expired sandboxes.
    pub reaper_interval: Duration,
    /// Default per-call timeout for tool calls.
    pub call_timeout: Duration,
    /// How long [`SandboxManager::shutdown`] waits for all teardowns (including ones
    /// already in flight) before reporting the stragglers as failed.
    pub shutdown_timeout: Duration,
    /// Host-local template pin store ([`crate::pins`]). `None` = [`PinStore::open_default`]
    /// (`$SANDCRAB_TEMPLATE_PINS`, else `<$SANDCRAB_HOME or ~/.sandcrab>/template-pins`).
    pub template_pins: Option<PinStore>,
    /// Allowed roots for [`SandboxConfig::host_mounts`]. `None`: every host mount is refused.
    pub mount_policy: Option<MountPolicy>,
}

impl Default for ManagerConfig {
    fn default() -> Self {
        Self {
            ready_timeout: Duration::from_secs(30),
            reaper_interval: Duration::from_secs(1),
            call_timeout: DEFAULT_CALL_TIMEOUT,
            shutdown_timeout: Duration::from_secs(60),
            template_pins: None,
            mount_policy: None,
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
    /// The manager runs on exactly the backend it is given (never another one); spawns
    /// that require hardware isolation are refused on a backend without it.
    pub fn new(
        backend: Arc<dyn RuntimeBackend>,
        workspaces: WorkspaceRegistry,
        templates: TemplateRegistry,
        config: ManagerConfig,
    ) -> Result<Self> {
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

    /// The host-local template pin store spawns verify against
    /// ([`ManagerConfig::template_pins`], else [`PinStore::open_default`]; `None` when no
    /// default location can be determined).
    #[must_use]
    pub fn pin_store(&self) -> Option<PinStore> {
        self.inner
            .config
            .template_pins
            .clone()
            .or_else(|| PinStore::open_default().ok())
    }

    /// The allowed roots for host mounts ([`ManagerConfig::mount_policy`]).
    #[must_use]
    pub fn mount_policy(&self) -> Option<&MountPolicy> {
        self.inner.config.mount_policy.as_ref()
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
            return Err(SandboxError::HardwareIsolationRequired(format!(
                "backend `{}` does not provide it",
                backend.kind()
            )));
        }
        if !config.secrets.is_empty() {
            return Err(SandboxError::PolicyUnenforceable(
                "secrets are reserved for Phase 3; no V1 backend can inject them".into(),
            ));
        }
        RequestedOptions {
            forwards: !config.forwards.is_empty(),
            host_mounts: !config.host_mounts.is_empty(),
            tmp_options: config.tmp_exec || config.tmp_size_mb.is_some(),
            process_limits: config.resources.has_process_limits(),
        }
        .check(backend.kind(), &caps)?;

        let template = inner.templates.get(&config.template)?.clone();
        let manifest = &template.manifest;
        let network = config
            .network
            .clone()
            .unwrap_or_else(|| manifest.network.to_config());
        network.validate()?;
        if !backend.enforces_network_policy(&network) {
            let hint = if let Some(h) = backend.network_policy_hint(&network) {
                format!(": {h}")
            } else if network.policy == NetworkPolicy::EgressRestricted {
                " (no V1 backend enforces egress_restricted; use `disabled` or `unrestricted`)"
                    .to_owned()
            } else {
                String::new()
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
        let host_mounts = if config.host_mounts.is_empty() {
            Vec::new()
        } else {
            let taken: Vec<&Path> = resolved
                .mounts
                .iter()
                .map(|m| m.guest_path.as_path())
                .collect();
            match &inner.config.mount_policy {
                Some(policy) => policy.resolve(&config.host_mounts, &taken)?,
                None => {
                    return Err(SandboxError::MountNotAllowed {
                        path: config.host_mounts[0].path.clone(),
                        reason: "no mount roots are configured".into(),
                        cap: None,
                    });
                }
            }
        };

        // Host-local pin (else a legacy in-manifest one). A corrupt pin file fails the spawn;
        // backends refuse a missing pin (`pins::unpinned_error`) unless explicitly opted out.
        let pin = effective_pin(self.pin_store().as_ref(), manifest, ImageArch::host())?;

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
            image_sha256: pin.map(|p| p.digest),
            mounts,
            default_cwd: resolved.default_cwd.clone(),
            env: config.env.clone(),
            network,
            ports: config.ports.clone(),
            user,
            resources,
            ttl: config.ttl_duration(),
            secrets: Vec::new(),
            forwards: config.forwards.clone(),
            host_mounts: host_mounts.clone(),
            tmp_exec: config.tmp_exec,
            tmp_size_mb: config.tmp_size_mb,
            pids_max: config.resources.pids_max,
            nofile: config.resources.nofile,
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
        // Never a sandbox whose forwards nobody listens for (design §12.5).
        if let Some(missing) = missing_forwards(&ready.hello, &config.forwards) {
            ready.client.close().await;
            if let Some(handle) = guard.0.take()
                && let Err(de) = handle.destroy().await
            {
                tracing::warn!(%id, error = %de, "destroy after a refused spawn also failed");
            }
            return Err(SandboxError::PolicyUnenforceable(missing));
        }
        // The backend handed mount paths to something that resolved them again: compare
        // what the sandbox got with what was opened (design §6.3).
        if caps.mount_identity_checked
            && let Err(e) = check_mount_identities(&ready.client, &spec).await
        {
            ready.client.close().await;
            if let Some(handle) = guard.0.take()
                && let Err(de) = handle.destroy().await
            {
                tracing::warn!(%id, error = %de, "destroy after a failed mount check also failed");
            }
            return Err(e);
        }
        let handle = guard.0.take().expect("handle present until readiness");
        let ports = handle.published_ports();

        let expires_at = config.ttl_duration().map(|ttl| Instant::now() + ttl);
        let sandbox = Sandbox {
            shared: Arc::new(SandboxShared {
                id,
                template: config.template,
                backend: backend.kind(),
                client: Arc::new(ready.client.with_default_timeout(inner.config.call_timeout)),
                connector,
                processes: ProcessTable::new(),
                acceptor: Mutex::new(None),
                hello: ready.hello,
                timings,
                ports,
                default_cwd: resolved.default_cwd,
                labels: config.labels,
                host_mounts: host_mounts
                    .into_iter()
                    .map(|m| HostMount::new(m.host_path, m.mode))
                    .collect(),
                forwards: config.forwards,
                created_at: Utc::now(),
                expires_at,
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

    /// Live sandboxes that match `filter`, oldest first.
    #[must_use]
    pub fn list(&self, filter: &SandboxFilter) -> Vec<SandboxInfo> {
        let mut all: Vec<SandboxInfo> = lock(&self.inner.active)
            .values()
            .filter(|s| filter.matches(s))
            .map(Sandbox::info)
            .collect();
        all.sort_by_key(|i| i.created_at);
        all
    }

    /// Destroy every sandbox whose labels include all of `labels` (not empty), then look
    /// again. `empty` is true only when every destroy succeeded and the second look finds no
    /// match; a sandbox created with these labels meanwhile makes it false.
    pub async fn destroy_by_label(
        &self,
        labels: &BTreeMap<String, String>,
    ) -> Result<DestroyByLabel> {
        if labels.is_empty() {
            return Err(SandboxError::InvalidConfig(
                "destroy_by_label needs at least one label".into(),
            ));
        }
        validate_labels(labels)?;
        let filter = SandboxFilter {
            labels: labels.clone(),
            ..SandboxFilter::default()
        };
        let matched: Vec<Sandbox> = lock(&self.inner.active)
            .values()
            .filter(|s| filter.matches(s))
            .cloned()
            .collect();
        let mut tasks = tokio::task::JoinSet::new();
        for s in &matched {
            let s = s.clone();
            tasks.spawn(async move { s.destroy().await.is_ok() });
        }
        let mut destroyed = 0;
        while let Some(r) = tasks.join_next().await {
            if r.unwrap_or(false) {
                destroyed += 1;
            }
        }
        let remaining: Vec<SandboxId> = self.list(&filter).iter().map(|i| i.id).collect();
        Ok(DestroyByLabel {
            matched: matched.len(),
            destroyed,
            empty: destroyed == matched.len() && remaining.is_empty(),
            remaining,
        })
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

/// Ask `crabd` (`fs.identity`) for the `(dev, ino)` of every directory mount point and compare
/// each with the identity recorded when the host directory was opened. Only meaningful where
/// the sandbox shares the host's kernel ([`BackendCapabilities::mount_identity_checked`]).
async fn check_mount_identities(client: &CrabdClient, spec: &ResolvedSpec) -> Result<()> {
    let mut paths = Vec::new();
    let mut expected = Vec::new();
    for m in spec.mounts.iter().chain(&spec.host_mounts) {
        let (true, Some(want)) = (m.backing.is_dir(), m.root_identity) else {
            continue;
        };
        let guest = m.guest_path.to_str().ok_or_else(|| {
            SandboxError::InvalidConfig(format!(
                "guest path `{}` is not UTF-8",
                m.guest_path.display()
            ))
        })?;
        paths.push(guest.to_owned());
        expected.push(want);
    }
    if paths.is_empty() {
        return Ok(());
    }
    let got = client.fs_identity(paths.clone()).await?;
    for ((path, want), got) in paths.iter().zip(expected).zip(got) {
        if (want.dev, want.ino) != (got.dev, got.ino) {
            return Err(SandboxError::WorkspaceRootChanged(format!(
                "mount `{path}`: the sandbox sees device/inode {}:{}, the directory that was                  checked is {}:{}; the sandbox was destroyed",
                got.dev, got.ino, want.dev, want.ino
            )));
        }
    }
    Ok(())
}

/// Why `crabd` cannot serve `forwards`, if it cannot: it does not speak forwards, or does not
/// listen on one of the ports.
fn missing_forwards(hello: &HelloResult, forwards: &[ForwardSpec]) -> Option<String> {
    if forwards.is_empty() {
        return None;
    }
    if !hello.features.iter().any(|f| f == features::FORWARD) {
        return Some("crabd in this sandbox does not serve forwards".into());
    }
    let missing: Vec<String> = forwards
        .iter()
        .filter(|f| !hello.forward_ports.contains(&f.guest_port))
        .map(|f| f.guest_port.to_string())
        .collect();
    (!missing.is_empty()).then(|| {
        format!(
            "crabd in this sandbox does not listen on forward port(s) {}",
            missing.join(", ")
        )
    })
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
    client: Arc<CrabdClient>,
    /// Opens the sandbox's dedicated connections (process, forwards); `auth` included.
    connector: Arc<dyn Connector>,
    processes: ProcessTable,
    /// Serves the forwards once a target was given ([`Sandbox::serve_forwards`]).
    acceptor: Mutex<Option<ForwardAcceptor>>,
    hello: HelloResult,
    timings: LifecycleTimings,
    ports: Vec<PublishedPort>,
    default_cwd: PathBuf,
    labels: BTreeMap<String, String>,
    host_mounts: Vec<HostMount>,
    forwards: Vec<ForwardSpec>,
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
        // Forwards end with the sandbox: dropping the acceptor cuts every relayed connection.
        drop(lock(&self.acceptor).take());
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

/// Which sandboxes [`SandboxManager::list`] returns. Every given label must match exactly;
/// `created_after` is inclusive, `created_before` exclusive.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SandboxFilter {
    pub labels: BTreeMap<String, String>,
    pub created_after: Option<DateTime<Utc>>,
    pub created_before: Option<DateTime<Utc>>,
}

impl SandboxFilter {
    fn matches(&self, sandbox: &Sandbox) -> bool {
        let s = &sandbox.shared;
        self.labels.iter().all(|(k, v)| s.labels.get(k) == Some(v))
            && self.created_after.is_none_or(|t| s.created_at >= t)
            && self.created_before.is_none_or(|t| s.created_at < t)
    }
}

/// Outcome of [`SandboxManager::destroy_by_label`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DestroyByLabel {
    /// Sandboxes that matched at the first look.
    pub matched: usize,
    /// Of those, how many were destroyed without error.
    pub destroyed: usize,
    /// Sandboxes that match at the second look.
    pub remaining: Vec<SandboxId>,
    /// Every destroy succeeded and nothing matches any more.
    pub empty: bool,
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
    pub labels: BTreeMap<String, String>,
    /// Host mounts, at their resolved paths (host path = guest path).
    pub host_mounts: Vec<HostMount>,
    pub forwards: Vec<ForwardSpec>,
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
    pub fn labels(&self) -> &BTreeMap<String, String> {
        &self.shared.labels
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
            labels: s.labels.clone(),
            host_mounts: s.host_mounts.clone(),
            forwards: s.forwards.clone(),
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

    /// Opens fresh transport connections to this sandbox's `crabd` (`auth` included).
    #[must_use]
    pub fn connector(&self) -> Arc<dyn Connector> {
        Arc::clone(&self.shared.connector)
    }

    fn require_live(&self) -> Result<()> {
        if self.is_destroyed() {
            return Err(SandboxError::Unavailable(format!(
                "sandbox {} is destroyed",
                self.id()
            )));
        }
        Ok(())
    }

    /// Run one program on a dedicated connection (`process.open`). See [`crate::process`]
    /// for the outcomes: [`SandboxError::Unavailable`] when nothing could be sent (also for a
    /// destroyed sandbox), the typed `crabd` error when it refused, [`SandboxError::Lost`]
    /// when the connection ended after the request. A `process_id` that is still open here
    /// is [`SandboxError::AlreadyExists`].
    pub async fn open_process(&self, params: ProcessOpenParams) -> Result<ProcessStream> {
        self.require_live()?;
        if !self
            .hello()
            .features
            .iter()
            .any(|f| f == features::PROCESS_OPEN)
        {
            return Err(SandboxError::Unimplemented(
                "crabd in this sandbox does not serve process.open",
            ));
        }
        self.shared
            .processes
            .open(
                self.shared.connector.as_ref(),
                &params,
                DEFAULT_PROCESS_OPEN_TIMEOUT,
            )
            .await
    }

    /// `process.cancel` on the tool connection: ask, wait `grace_ms` (default 5000, 0 =
    /// force at once), force. Answers when nothing of the tree is left. A cancel that races
    /// an [`Self::open_process`] of the same id always ends that process
    /// ([`ProcessTable::cancel`]).
    pub async fn cancel_process(
        &self,
        process_id: &str,
        grace_ms: Option<u64>,
    ) -> Result<ProcessCancelResult> {
        self.require_live()?;
        self.shared
            .processes
            .cancel(&self.shared.client, process_id, grace_ms)
            .await
    }

    /// `fs.identity`: `(dev, ino)` of each absolute guest path, in order.
    pub async fn fs_identity(&self, paths: Vec<String>) -> Result<Vec<FileIdentity>> {
        self.require_live()?;
        self.shared.client.fs_identity(paths).await
    }

    /// `forward.stats`: per forward port, connections accepted and refused since start.
    pub async fn forward_stats(&self) -> Result<Vec<ForwardPortStats>> {
        self.require_live()?;
        self.shared.client.forward_stats().await
    }

    /// Start serving this sandbox's forwards: each connection a program makes to a forward
    /// port goes to `target` ([`ForwardAcceptor`]). Once per sandbox; it stops when the
    /// sandbox is destroyed.
    pub fn serve_forwards(&self, target: Arc<dyn ForwardTarget>) -> Result<()> {
        self.require_live()?;
        let mut slot = lock(&self.shared.acceptor);
        if slot.is_some() {
            return Err(SandboxError::AlreadyExists {
                kind: "forward acceptor",
                id: self.id().to_string(),
            });
        }
        *slot = Some(ForwardAcceptor::start(
            self.id(),
            Arc::clone(&self.shared.connector),
            &self.shared.forwards,
            target,
            ForwardConfig::default(),
            None,
        )?);
        drop(slot);
        // A teardown that started meanwhile did not see the acceptor: stop it now.
        if self.is_destroyed() {
            drop(lock(&self.shared.acceptor).take());
        }
        Ok(())
    }

    /// Whether the forwards are served and the guest's listen connection is up.
    #[must_use]
    pub fn forwards_listening(&self) -> bool {
        lock(&self.shared.acceptor)
            .as_ref()
            .is_some_and(ForwardAcceptor::is_listening)
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
            fs::write(d.join("Dockerfile"), "FROM scratch\n").unwrap();
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
            // Hermetic: never read the host's real pin store.
            template_pins: Some(PinStore::open("/nonexistent/sandcrab-test-pins")),
            ..ManagerConfig::default()
        }
    }

    #[tokio::test]
    async fn spawn_passes_the_host_local_pin() {
        use crate::pins::{PinSource, TemplatePin};
        let env = env();
        let backend = FakeBackend::new();
        // Unpinned: the backend sees `None` (and decides to refuse).
        let mgr = manager(&env, &backend, quick());
        mgr.spawn(SandboxConfig::from_template("base"))
            .await
            .unwrap();
        assert_eq!(backend.last_spec().unwrap().image_sha256, None);

        let Some(arch) = ImageArch::host() else {
            return;
        };
        let store = PinStore::open(env._tmp.path().join("pins"));
        let digest = format!("sha256:{}", "d".repeat(64));
        let m = &env.templates.get("base").unwrap().manifest;
        store
            .record(&TemplatePin::new(
                m,
                arch,
                "sandcrab/base:0.1.0",
                &digest,
                PinSource::Build,
            ))
            .unwrap();
        let mgr = manager(
            &env,
            &backend,
            ManagerConfig {
                template_pins: Some(store),
                ..quick()
            },
        );
        mgr.spawn(SandboxConfig::from_template("base"))
            .await
            .unwrap();
        assert_eq!(backend.last_spec().unwrap().image_sha256, Some(digest));
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
    async fn shared_kernel_backend_runs_but_fails_hardware_requirement() {
        let env = env();
        let backend = FakeBackend::shared_kernel();
        // No opt-in needed for a shared-kernel backend ...
        let mgr = manager(&env, &backend, quick());
        assert_eq!(mgr.backend_kind(), BackendKind::Docker);
        assert!(!mgr.backend_capabilities().hardware_isolated);
        mgr.spawn(SandboxConfig::from_template("base"))
            .await
            .unwrap();
        let err = mgr
            .spawn(SandboxConfig::from_template("base").require_hardware_isolation(true))
            .await
            .unwrap_err();
        // ... but an explicit hardware requirement is an error, not a downgrade.
        assert!(
            matches!(err, SandboxError::HardwareIsolationRequired(_)),
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

    #[tokio::test]
    async fn labels_list_filters() {
        let env = env();
        let backend = FakeBackend::new();
        let mgr = manager(&env, &backend, quick());
        let a = mgr
            .spawn(
                SandboxConfig::from_template("base")
                    .label("run", "r1")
                    .label("kind", "check"),
            )
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        let since = Utc::now();
        let b = mgr
            .spawn(SandboxConfig::from_template("base").label("run", "r1"))
            .await
            .unwrap();
        let c = mgr
            .spawn(SandboxConfig::from_template("base").label("run", "r2"))
            .await
            .unwrap();
        assert_eq!(a.labels().get("kind").map(String::as_str), Some("check"));
        let ids = |f: SandboxFilter| mgr.list(&f).iter().map(|i| i.id).collect::<Vec<_>>();
        let by = |pairs: &[(&str, &str)]| SandboxFilter {
            labels: pairs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
            ..Default::default()
        };
        assert_eq!(ids(SandboxFilter::default()), [a.id(), b.id(), c.id()]);
        assert_eq!(ids(by(&[("run", "r1")])), [a.id(), b.id()]);
        assert_eq!(ids(by(&[("run", "r1"), ("kind", "check")])), [a.id()]);
        assert!(ids(by(&[("run", "r3")])).is_empty());
        assert_eq!(
            ids(SandboxFilter {
                created_after: Some(since),
                ..by(&[("run", "r1")])
            }),
            [b.id()]
        );
        assert_eq!(
            ids(SandboxFilter {
                created_before: Some(since),
                ..Default::default()
            }),
            [a.id()]
        );
        assert_eq!(mgr.list(&by(&[("run", "r2")]))[0].labels["run"], "r2");

        // Invalid labels never reach the backend.
        let n = backend.events().len();
        assert!(matches!(
            mgr.spawn(SandboxConfig::from_template("base").label("sandcrab.x", "v"))
                .await,
            Err(SandboxError::InvalidConfig(_))
        ));
        assert_eq!(backend.events().len(), n);
    }

    #[tokio::test]
    async fn destroy_by_label_confirms_empty() {
        let env = env();
        let backend = FakeBackend::new();
        let mgr = manager(&env, &backend, quick());
        let labels: BTreeMap<String, String> = [("run".to_owned(), "r1".to_owned())].into();
        for _ in 0..2 {
            mgr.spawn(SandboxConfig::from_template("base").label("run", "r1"))
                .await
                .unwrap();
        }
        let other = mgr
            .spawn(SandboxConfig::from_template("base").label("run", "r2"))
            .await
            .unwrap();
        let r = mgr.destroy_by_label(&labels).await.unwrap();
        assert_eq!(
            r,
            DestroyByLabel {
                matched: 2,
                destroyed: 2,
                remaining: Vec::new(),
                empty: true
            }
        );
        assert_eq!(mgr.list_active().len(), 1);
        assert!(!other.is_destroyed());
        // Nothing matches: empty, nothing destroyed.
        let r = mgr.destroy_by_label(&labels).await.unwrap();
        assert!(r.empty && r.matched == 0);
        assert!(matches!(
            mgr.destroy_by_label(&BTreeMap::new()).await,
            Err(SandboxError::InvalidConfig(_))
        ));

        // A failed destroy is never reported as empty.
        mgr.spawn(SandboxConfig::from_template("base").label("run", "r1"))
            .await
            .unwrap();
        backend.fail_destroy.store(true, Ordering::SeqCst);
        let r = mgr.destroy_by_label(&labels).await.unwrap();
        assert_eq!((r.matched, r.destroyed, r.empty), (1, 0, false));
    }

    #[tokio::test]
    async fn unsupported_options_are_refused_explicitly() {
        let env = env();
        let mut backend = FakeBackend::new();
        backend.caps.supports_forwards = false;
        backend.caps.supports_host_mounts = false;
        backend.caps.supports_tmp_options = false;
        backend.caps.supports_process_limits = false;
        let policy = crate::MountPolicy::new(
            vec![crate::MountRoot {
                path: env.project.clone(),
                max_mode: MountMode::ReadWrite,
            }],
            Vec::new(),
        )
        .unwrap();
        let mgr = manager(
            &env,
            &backend,
            ManagerConfig {
                mount_policy: Some(policy),
                ..quick()
            },
        );
        let base = SandboxConfig::from_template("base");
        for (cfg, what) in [
            (base.clone().forward(5432), "forwards"),
            (
                base.clone().host_mount(&env.project, MountMode::ReadOnly),
                "host mounts",
            ),
            (base.clone().tmp_exec(true), "tmp_exec"),
            (base.clone().tmp_size_mb(64), "tmp_size_mb"),
            (
                base.clone().resources(ResourceOverrides {
                    pids_max: Some(64),
                    ..Default::default()
                }),
                "pids_max",
            ),
            (
                base.clone().resources(ResourceOverrides {
                    nofile: Some(1024),
                    ..Default::default()
                }),
                "nofile",
            ),
        ] {
            let err = mgr.spawn(cfg).await.unwrap_err();
            assert!(
                matches!(&err, SandboxError::PolicyUnenforceable(m)
                    if m.contains(what) && m.contains("firecracker backend")),
                "{what}: {err}"
            );
        }
        assert_eq!(
            mgr.spawn(base.forward(5432)).await.unwrap_err().to_string(),
            "policy cannot be enforced: forwards are not supported by the firecracker backend"
        );
        assert!(backend.events().is_empty());
    }

    #[tokio::test]
    async fn mount_identity_is_checked_after_start() {
        let env = env();
        let mut backend = FakeBackend::shared_kernel();
        backend.caps.mount_identity_checked = true;
        let other = fs::canonicalize(env._tmp.path()).unwrap().join("other");
        fs::create_dir(&other).unwrap();
        let policy = crate::MountPolicy::new(
            vec![crate::MountRoot {
                path: other.clone(),
                max_mode: MountMode::ReadWrite,
            }],
            Vec::new(),
        )
        .unwrap();
        let mgr = manager(
            &env,
            &backend,
            ManagerConfig {
                mount_policy: Some(policy),
                ..quick()
            },
        );
        // The fake `crabd` answers with the host's numbers for a path: a workspace seen at
        // its own path and a host mount (same path by definition) pass.
        let sb = mgr
            .spawn(
                SandboxConfig::from_template("base")
                    .mount("proj", &env.project, MountMode::ReadWrite)
                    .host_mount(&other, MountMode::ReadOnly),
            )
            .await
            .unwrap();
        sb.destroy().await.unwrap();

        // A mount point that shows another directory fails the create and leaves nothing.
        let before = backend.events().len();
        let err = mgr
            .spawn(SandboxConfig::from_template("base").mount("proj", &other, MountMode::ReadWrite))
            .await
            .unwrap_err();
        assert!(
            matches!(&err, SandboxError::WorkspaceRootChanged(m) if m.contains("device/inode")),
            "{err:?}"
        );
        let events = backend.events();
        assert!(
            matches!(&events[before..], [FakeEvent::Spawned(a), FakeEvent::Destroyed(b)] if a == b),
            "{events:?}"
        );
        assert!(mgr.list_active().is_empty());
        // The lease is released: the workspace can be mounted again.
        let sb = mgr
            .spawn(SandboxConfig::from_template("base").mount(
                "proj",
                &env.project,
                MountMode::ReadWrite,
            ))
            .await
            .unwrap();
        sb.destroy().await.unwrap();

        // A backend that does not declare the check is not asked.
        backend.caps.mount_identity_checked = false;
        let mgr = manager(&env, &backend, quick());
        mgr.spawn(SandboxConfig::from_template("base").mount("proj", &other, MountMode::ReadWrite))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn new_options_reach_the_backend() {
        let env = env();
        let backend = FakeBackend::shared_kernel();
        let data = env._tmp.path().join("data");
        fs::create_dir_all(data.join("sub")).unwrap();
        let data = fs::canonicalize(data).unwrap();
        let policy = crate::MountPolicy::new(
            vec![crate::MountRoot {
                path: data.clone(),
                max_mode: MountMode::ReadWrite,
            }],
            Vec::new(),
        )
        .unwrap();

        // No policy: every host mount is refused.
        let mgr = manager(&env, &backend, quick());
        assert!(matches!(
            mgr.spawn(SandboxConfig::from_template("base").host_mount(&data, MountMode::ReadOnly))
                .await,
            Err(SandboxError::MountNotAllowed { .. })
        ));

        let mgr = manager(
            &env,
            &backend,
            ManagerConfig {
                mount_policy: Some(policy),
                ..quick()
            },
        );
        let sb = mgr
            .spawn(
                SandboxConfig::from_template("base")
                    .mount("proj", "/workspace", MountMode::ReadWrite)
                    .host_mount(data.join("sub"), MountMode::ReadWrite)
                    .forward(5432)
                    .tmp_exec(true)
                    .tmp_size_mb(512)
                    .resources(ResourceOverrides {
                        pids_max: Some(256),
                        nofile: Some(4096),
                        ..Default::default()
                    }),
            )
            .await
            .unwrap();
        let spec = backend.last_spec().unwrap();
        assert_eq!(spec.host_mounts.len(), 1);
        assert_eq!(spec.host_mounts[0].host_path, data.join("sub"));
        assert_eq!(spec.host_mounts[0].guest_path, data.join("sub"));
        assert!(spec.host_mounts[0].root_identity.is_some());
        assert_eq!(spec.mounts.len(), 1);
        assert_eq!(spec.forwards, [crate::ForwardSpec::new(5432)]);
        assert!(spec.tmp_exec);
        assert_eq!(spec.tmp_size_mb, Some(512));
        assert_eq!((spec.pids_max, spec.nofile), (Some(256), Some(4096)));
        let info = sb.info();
        assert_eq!(
            info.host_mounts,
            [HostMount::new(data.join("sub"), MountMode::ReadWrite)]
        );
        assert_eq!(info.forwards.len(), 1);

        // A second sandbox may mount a folder nested with the first one's.
        mgr.spawn(SandboxConfig::from_template("base").host_mount(&data, MountMode::ReadOnly))
            .await
            .unwrap();
        // Within one sandbox, a host mount may not nest with a workspace mount.
        let err = mgr
            .spawn(
                SandboxConfig::from_template("base")
                    .mount("proj", data.join("sub/x"), MountMode::ReadOnly)
                    .host_mount(data.join("sub"), MountMode::ReadOnly),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, SandboxError::MountNotAllowed { .. }),
            "{err:?}"
        );
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

    /// Echo target for forwards.
    struct EchoTarget;

    #[async_trait::async_trait]
    impl crate::ForwardTarget for EchoTarget {
        async fn connect(&self, _: SandboxId, _: u16) -> std::io::Result<crate::BoxedStream> {
            let (a, b) = tokio::io::duplex(1 << 16);
            tokio::spawn(async move {
                let (mut r, mut w) = tokio::io::split(b);
                let _ = tokio::io::copy(&mut r, &mut w).await;
                let _ = tokio::io::AsyncWriteExt::shutdown(&mut w).await;
            });
            Ok(Box::new(a))
        }
    }

    #[tokio::test]
    async fn processes_through_the_sandbox() {
        use crate::ProcessEvent;
        let env = env();
        let backend = FakeBackend::new();
        let mgr = manager(&env, &backend, quick());
        let sb = mgr
            .spawn(SandboxConfig::from_template("base"))
            .await
            .unwrap();
        let mock = backend.mock_of(sb.id()).unwrap();

        let mut p = ProcessOpenParams::new(uuid::Uuid::new_v4().to_string(), vec!["env".into()]);
        p.env.insert("A".into(), "1".into());
        let mut ps = sb.open_process(p).await.unwrap();
        assert_eq!(
            ps.output().next().await.unwrap(),
            Some(ProcessEvent::Stdout(b"A=1\n".to_vec()))
        );
        assert!(matches!(
            ps.output().next().await.unwrap(),
            Some(ProcessEvent::Exit(e)) if e.code == Some(0)
        ));

        let id = uuid::Uuid::new_v4().to_string();
        let mut ps = sb
            .open_process(ProcessOpenParams::new(id.clone(), vec!["sleep".into()]))
            .await
            .unwrap();
        let r = sb.cancel_process(&id, Some(0)).await.unwrap();
        assert!(r.was_running);
        assert!(matches!(
            ps.output().next().await.unwrap(),
            Some(ProcessEvent::Exit(e)) if e.cancelled
        ));
        assert_eq!(mock.processes.opened.load(Ordering::SeqCst), 2);

        // fs.identity answers what the kernel says about the path.
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let path = env.project.to_str().unwrap().to_owned();
            let ids = sb.fs_identity(vec![path]).await.unwrap();
            let m = fs::metadata(&env.project).unwrap();
            assert_eq!((ids[0].dev, ids[0].ino), (m.dev(), m.ino()));
        }

        // Destroyed: nothing runs, nothing could have.
        let open = sb.open_process(ProcessOpenParams::new(
            uuid::Uuid::new_v4().to_string(),
            vec!["sleep".into()],
        ));
        let ps = open.await.unwrap();
        sb.destroy().await.unwrap();
        drop(ps);
        let err = sb
            .open_process(ProcessOpenParams::new(
                uuid::Uuid::new_v4().to_string(),
                vec!["echo".into()],
            ))
            .await
            .unwrap_err();
        assert!(matches!(err, SandboxError::Unavailable(_)), "{err:?}");
    }

    #[tokio::test]
    async fn forwards_through_the_sandbox() {
        let env = env();
        let backend = FakeBackend::new();
        let mgr = manager(&env, &backend, quick());
        let sb = mgr
            .spawn(SandboxConfig::from_template("base").forward(5432))
            .await
            .unwrap();
        let fwd = std::sync::Arc::clone(&backend.mock_of(sb.id()).unwrap().forwards);
        sb.serve_forwards(Arc::new(EchoTarget)).unwrap();
        assert!(matches!(
            sb.serve_forwards(Arc::new(EchoTarget)),
            Err(SandboxError::AlreadyExists { .. })
        ));
        let deadline = Instant::now() + Duration::from_secs(5);
        while !sb.forwards_listening() {
            assert!(Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let mut p = fwd.connect(5432).unwrap();
        tokio::io::AsyncWriteExt::write_all(&mut p, b"hi")
            .await
            .unwrap();
        tokio::io::AsyncWriteExt::shutdown(&mut p).await.unwrap();
        let mut back = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(&mut p, &mut back)
            .await
            .unwrap();
        assert_eq!(back, b"hi");
        let stats = sb.forward_stats().await.unwrap();
        assert_eq!(stats.len(), 1);
        assert_eq!((stats[0].port, stats[0].accepted), (5432, 1));

        // The forwards end with the sandbox.
        sb.destroy().await.unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while fwd.has_listener() {
            assert!(
                Instant::now() < deadline,
                "listen connection outlived the sandbox"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        // A sandbox without forwards has nothing to serve.
        let plain = mgr
            .spawn(SandboxConfig::from_template("base"))
            .await
            .unwrap();
        assert!(matches!(
            plain.serve_forwards(Arc::new(EchoTarget)),
            Err(SandboxError::InvalidConfig(_))
        ));
    }

    #[tokio::test]
    async fn spawn_refuses_a_crabd_that_does_not_serve_the_forwards() {
        let env = env();
        let mut backend = FakeBackend::new();
        backend.mock.features.clear();
        let mgr = manager(&env, &backend, quick());
        let err = mgr
            .spawn(SandboxConfig::from_template("base").forward(5432))
            .await
            .unwrap_err();
        assert!(
            matches!(&err, SandboxError::PolicyUnenforceable(m) if m.contains("forwards")),
            "{err}"
        );
        assert!(matches!(
            backend.events().last(),
            Some(FakeEvent::Destroyed(_))
        ));
        // Without forwards it spawns, but process.open is refused up front.
        let sb = mgr
            .spawn(SandboxConfig::from_template("base"))
            .await
            .unwrap();
        assert!(matches!(
            sb.open_process(ProcessOpenParams::new(
                uuid::Uuid::new_v4().to_string(),
                vec!["echo".into()]
            ))
            .await,
            Err(SandboxError::Unimplemented(_))
        ));
    }

    #[tokio::test]
    async fn spawn_refuses_a_0_2_guest() {
        let env = env();
        let mut backend = FakeBackend::new();
        backend.mock.protocol_version = "0.2".into();
        let mgr = manager(&env, &backend, quick());
        let err = mgr
            .spawn(SandboxConfig::from_template("base"))
            .await
            .unwrap_err();
        assert!(
            matches!(&err, SandboxError::Protocol(ProtocolError::UnsupportedVersion { crabd, host })
                if crabd == "0.2" && host == sandcrab_protocol::PROTOCOL_VERSION),
            "{err:?}"
        );
    }
}
