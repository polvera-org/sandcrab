//! `sandcrab-server`: the Sandcrab host daemon (design `docs/design/server-and-sdk.md` §5).
//!
//! One explicitly chosen backend (built by the caller on a [`SandcrabBuilder`], never a
//! fallback), a Unix socket in a private directory, the peer uid checked on every connection,
//! and a length-prefixed JSON-RPC API ([`sandcrab_sdk::protocol::api`]).
//!
//! ```text
//! lib.rs        Server: start, run, the shared state
//! config.rs     ServerConfig (TOML + flags)
//! lifecycle.rs  start (directories, mount policy, backend, predecessor removal), stop, lifeline
//! listener.rs   the socket (directory check, stale socket, mode and group) and the first frame
//! peer.rs       peer credentials and the uid check
//! control.rs    a control connection: `server.hello`, then the sandbox calls
//! process.rs    a process connection (`process.spawn`), the process table, `process.cancel`
//! forwards.rs   control connections and forwards: `forward.incoming`, `forward.attach`
//! wire.rs       async frames
//! ```
//!
//! Sandboxes belong to the peer uid that created it: a uid sees and touches only its own.
//! They outlive client connections; a server stop destroys them, and a server start removes
//! what a dead predecessor left (Docker, owned containers only).

pub mod config;
mod control;
mod forwards;
mod lifecycle;
mod listener;
pub mod peer;
mod process;
mod wire;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, PoisonError};

use sandcrab_sdk::protocol::api::ServerLimits;
use sandcrab_sdk::{SandboxError, SandboxId, Sandcrab};
use tokio::net::UnixListener;

pub use config::{BackendName, LimitsConfig, MountRootConfig, ServerConfig};
pub use lifecycle::{lifeline, stdin_lifeline};
pub use peer::Peer;

/// Connections of all kinds open at once.
pub const MAX_CONNECTIONS: u32 = 1024;
/// Requests in flight per control connection; beyond that the server stops reading it.
pub const MAX_REQUESTS_IN_FLIGHT: u32 = 64;
/// Processes per sandbox (the guest's `--max-processes`).
pub const MAX_PROCESSES_PER_SANDBOX: u32 = 64;
/// Per-sandbox resource ceilings (design §5.6).
pub const MAX_VCPU: u32 = 8;
pub const MAX_MEMORY_MB: u64 = 16384;
pub const MAX_PIDS: u64 = 8192;
pub const MAX_NOFILE: u64 = 1_048_576;
/// A connection that sends no first frame within this is closed.
pub const FIRST_FRAME_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Why the server could not start or stop cleanly.
#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    #[error("server config: {0}")]
    Config(String),
    #[error(transparent)]
    Sandbox(#[from] SandboxError),
    #[error("{context}: {source}")]
    Io {
        context: String,
        source: std::io::Error,
    },
}

impl ServerError {
    pub(crate) fn io(context: impl Into<String>, source: std::io::Error) -> Self {
        Self::Io {
            context: context.into(),
            source,
        }
    }
}

/// A started server: the backend is up, a predecessor's sandboxes are gone and the socket
/// accepts. [`Self::run`] serves until its shutdown future resolves.
pub struct Server {
    shared: Arc<Shared>,
    listener: UnixListener,
    socket: listener::BoundSocket,
}

impl std::fmt::Debug for Server {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Server")
            .field("socket", &self.socket.path)
            .field("backend", &self.shared.backend)
            .finish_non_exhaustive()
    }
}

impl Server {
    /// Start a server. `builder` carries the backend (and nothing else is needed on it);
    /// the server sets the state directory, the template directories and the mount policy
    /// from `config`. `protected` adds directories no mount may touch (the backend's own run
    /// directories); the socket directory, the state directory and the pin store are always
    /// protected.
    pub async fn start(
        config: ServerConfig,
        builder: sandcrab_sdk::SandcrabBuilder,
        protected: Vec<PathBuf>,
    ) -> Result<Self, ServerError> {
        lifecycle::start(config, builder, protected).await
    }

    /// The socket clients connect to.
    #[must_use]
    pub fn socket_path(&self) -> &Path {
        &self.socket.path
    }

    /// The backend handle (for tests and diagnostics).
    #[must_use]
    pub fn sandcrab(&self) -> &Sandcrab {
        &self.shared.sandcrab
    }

    /// Serve until `shutdown` resolves (a signal, the stdin lifeline, a test). Then refuse new
    /// requests, destroy every sandbox and remove the socket.
    pub async fn run(
        self,
        shutdown: impl std::future::Future<Output = ()>,
    ) -> Result<(), ServerError> {
        lifecycle::run(self, shutdown).await
    }
}

/// State every connection shares.
pub(crate) struct Shared {
    pub sandcrab: Sandcrab,
    pub config: ServerConfig,
    pub allow_uids: Vec<u32>,
    /// The backend's name as `server.hello` reports it.
    pub backend: String,
    pub limits: ServerLimits,
    /// Who created each sandbox. A sandbox missing here is still being created (or not this
    /// server's): no client sees it.
    owners: Mutex<HashMap<SandboxId, u32>>,
    /// Creates in flight, counted against `max_sandboxes`.
    creating: Mutex<usize>,
    pub shutting_down: AtomicBool,
    /// Control connections, who serves which forwards, announced forwarded connections.
    pub hub: Arc<forwards::Hub>,
    /// Process connections by `process_id`.
    pub processes: process::Processes,
}

impl Shared {
    pub(crate) fn new(sandcrab: Sandcrab, config: ServerConfig, allow_uids: Vec<u32>) -> Self {
        let backend = sandcrab.backend_kind().to_string();
        let limits = ServerLimits {
            max_connections: MAX_CONNECTIONS,
            max_requests_in_flight: MAX_REQUESTS_IN_FLIGHT,
            max_sandboxes: config.limits.max_sandboxes,
            max_processes_per_sandbox: MAX_PROCESSES_PER_SANDBOX,
            max_mounts: u32::try_from(sandcrab_sdk::core::MAX_SANDBOX_MOUNTS).unwrap_or(16),
            max_vcpu: MAX_VCPU,
            max_memory_mb: MAX_MEMORY_MB,
            max_pids: MAX_PIDS,
            max_nofile: MAX_NOFILE,
            default_ttl_secs: config.limits.default_ttl_secs,
            max_ttl_secs: config.limits.max_ttl_secs,
        };
        Self {
            sandcrab,
            config,
            allow_uids,
            backend,
            limits,
            owners: Mutex::default(),
            creating: Mutex::new(0),
            shutting_down: AtomicBool::new(false),
            hub: Arc::default(),
            processes: process::Processes::default(),
        }
    }

    fn owners(&self) -> std::sync::MutexGuard<'_, HashMap<SandboxId, u32>> {
        self.owners.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Whether `uid` created sandbox `id`.
    pub(crate) fn owns(&self, uid: u32, id: SandboxId) -> bool {
        self.owners().get(&id) == Some(&uid)
    }

    pub(crate) fn set_owner(&self, id: SandboxId, uid: u32) {
        self.owners().insert(id, uid);
    }

    pub(crate) fn forget(&self, id: SandboxId) {
        self.owners().remove(&id);
    }

    /// Drop owner records (and who served their forwards) of sandboxes that are gone (TTL,
    /// backend exit).
    pub(crate) fn prune_owners(&self) {
        let live: std::collections::HashSet<SandboxId> = self
            .sandcrab
            .list_sandboxes()
            .into_iter()
            .map(|i| i.id)
            .collect();
        self.owners().retain(|id, _| {
            let keep = live.contains(id);
            if !keep {
                self.hub.forget(*id);
            }
            keep
        });
    }

    /// Take a sandbox slot or answer `LIMIT_EXCEEDED`. The slot is returned when the guard
    /// drops; by then the sandbox is live (and counted as such) or was never made.
    pub(crate) fn reserve_slot(
        &self,
    ) -> Result<SlotGuard<'_>, sandcrab_sdk::protocol::api::ApiError> {
        let mut creating = self.creating.lock().unwrap_or_else(PoisonError::into_inner);
        let live = self.sandcrab.list_sandboxes().len();
        let max = self.config.limits.max_sandboxes as usize;
        if live + *creating >= max {
            return Err(sandcrab_sdk::protocol::api::ApiError::LimitExceeded(
                sandcrab_sdk::protocol::api::LimitExceededData {
                    what: "sandboxes".into(),
                    limit: u64::from(self.config.limits.max_sandboxes),
                },
            ));
        }
        *creating += 1;
        Ok(SlotGuard(self))
    }
}

pub(crate) struct SlotGuard<'a>(&'a Shared);

impl Drop for SlotGuard<'_> {
    fn drop(&mut self) {
        let mut c = self
            .0
            .creating
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        *c = c.saturating_sub(1);
    }
}
