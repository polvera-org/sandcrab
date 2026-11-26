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
//! selftest.rs   `server.selftest`: one run at a time per template, the last report kept
//! watch.rs      `sandbox.destroyed` for sandboxes the TTL reaper destroyed
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
mod selftest;
mod watch;
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

/// Connections of all kinds open at once, at most (less when the fd limit is low, see
/// [`fd_caps`]).
pub const MAX_CONNECTIONS: u32 = 1024;
/// Forwarded connections relayed at once, all sandboxes together, at most (less when the fd
/// limit is low). Each holds two descriptors (the attach socket and the guest connection);
/// one past the cap is refused, so the guest closes the program's connection.
pub const MAX_FORWARDED_CONNECTIONS: u32 = 4096;
/// The soft `RLIMIT_NOFILE` the server asks for at start (up to the hard limit).
pub const WANT_NOFILE: u64 = 65_536;
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

/// Raise the soft `RLIMIT_NOFILE` towards the hard limit (at most [`WANT_NOFILE`]) and
/// return the soft limit in force.
pub fn raise_nofile() -> u64 {
    use rustix::process::{Resource, getrlimit, setrlimit};
    let cur = getrlimit(Resource::Nofile);
    let soft = cur.current.unwrap_or(WANT_NOFILE);
    let want = cur.maximum.map_or(WANT_NOFILE, |h| h.min(WANT_NOFILE));
    // macOS refuses more than `kern.maxfilesperproc`; 10240 (`OPEN_MAX`) is always allowed.
    for want in [want, want.min(10_240)] {
        if want <= soft {
            break;
        }
        let mut raised = cur;
        raised.current = Some(want);
        match setrlimit(Resource::Nofile, raised) {
            Ok(()) => return want,
            Err(e) => tracing::warn!(error = %e, soft, want, "cannot raise RLIMIT_NOFILE"),
        }
    }
    soft
}

/// `(max_connections, max_forwarded)` for a soft fd limit of `nofile`: connections (two
/// fds each at most: the client and the guest) take up to half the limit, forwarded
/// connections (two each) a quarter, the rest is for the backend, sandboxes and logs. So
/// the caps refuse with `LIMIT_EXCEEDED` before `accept` or a dial meets `EMFILE`.
#[must_use]
pub fn fd_caps(nofile: u64) -> (u32, u32) {
    let part = |div: u64, max: u32| u32::try_from(nofile / div).unwrap_or(max).clamp(1, max);
    (part(4, MAX_CONNECTIONS), part(8, MAX_FORWARDED_CONNECTIONS))
}

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
    /// Self-test runs and the last report per template.
    pub selftests: selftest::Selftests,
    /// Last seen expiry of each sandbox, to tell a TTL reap from a destroy.
    pub expiries: watch::Expiries,
}

impl Shared {
    pub(crate) fn new(sandcrab: Sandcrab, config: ServerConfig, allow_uids: Vec<u32>) -> Self {
        let backend = sandcrab.backend_kind().to_string();
        let (max_connections, max_forwarded) = fd_caps(raise_nofile());
        let limits = ServerLimits {
            max_connections,
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
            hub: Arc::new(forwards::Hub::new(max_forwarded)),
            processes: process::Processes::default(),
            selftests: selftest::Selftests::default(),
            expiries: watch::Expiries::default(),
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
    /// backend exit); announce the expired ones.
    pub(crate) fn prune_owners(&self) {
        self.expiries.sweep(self);
    }

    /// The sandboxes that have an owner now.
    pub(crate) fn owned_ids(&self) -> Vec<SandboxId> {
        self.owners().keys().copied().collect()
    }

    /// Remove the owner records of `ids`; returns those that were there, with their uid.
    pub(crate) fn take_owners(&self, ids: &[SandboxId]) -> Vec<(SandboxId, u32)> {
        let mut owners = self.owners();
        ids.iter()
            .filter_map(|id| owners.remove(id).map(|uid| (*id, uid)))
            .collect()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fd_caps_fit_the_fd_limit() {
        assert_eq!(
            fd_caps(65_536),
            (MAX_CONNECTIONS, MAX_FORWARDED_CONNECTIONS)
        );
        assert_eq!(fd_caps(1024), (256, 128));
        assert_eq!(fd_caps(0), (1, 1));
        for n in [256, 1024, 4096, 20_000] {
            let (c, f) = fd_caps(n);
            // Two descriptors each at most, with a quarter left for everything else.
            assert!(2 * u64::from(c) + 2 * u64::from(f) <= n * 3 / 4, "{n}");
        }
    }

    #[test]
    fn raise_nofile_reaches_the_hard_limit() {
        let soft = raise_nofile();
        let cur = rustix::process::getrlimit(rustix::process::Resource::Nofile);
        assert_eq!(cur.current, Some(soft));
        #[cfg(target_os = "linux")]
        if let Some(hard) = cur.maximum {
            assert!(soft >= hard.min(WANT_NOFILE), "soft {soft}, hard {hard}");
        }
    }
}
