//! Firecracker microVM backend for Sandcrab (design: `docs/design/firecracker-backend.md`).
//!
//! [`FirecrackerBackend`] is a hardware-isolated [`sandcrab_core::RuntimeBackend`]: one
//! unprivileged `firecracker` process per sandbox (built-in seccomp on, no jailer in V1, so
//! `multi_tenant_safe = false`), booting the pinned 6.18 guest kernel with `crabd` as PID 1.
//!
//! Modules:
//! - [`api`]: owned HTTP/1.1-over-UDS client for the Firecracker API (no hyper).
//! - [`vsock`]: `CONNECT <port>` / `OK` handshake on the VMM's vsock UDS, as a core
//!   [`sandcrab_core::Connector`].
//! - [`control`]: host client of the guest `init.*` control channel (vsock port 1).
//! - [`workspace`]: host dir → ext4 drive (rw per spawn, ro cached per generation),
//!   sync-back via a hostile-archive-safe unpack into staging + commit.
//! - [`net`]: user-owned tap pool for `unrestricted` networking (`scripts/fc-net-setup.sh`).
//! - [`rootfs`]: OCI image → read-only ext4 rootfs pipeline and per-sandbox scratch drives.
//! - [`artifacts`]: pinned guest kernel and Firecracker release binaries (SHA-256 verified).
//! - [`reap`]: orphan reaping of run dirs left by dead sandcrab processes.
//!
//! Lifecycle of one sandbox (timing marks in parentheses, see the bench spec):
//! run dir `<run_dir>/<id>/` → rootfs (cache) + sparse scratch (`rootfs_ready`) → workspace
//! images (`ws_attach_start`/`ws_attach_done`) → tap lease (`unrestricted` only) →
//! `firecracker --api-sock` (`vmm_started`) → pre-boot PUTs (`configured`) → `InstanceStart`
//! (`instance_start` = `BackendStarted`) → control port (`control_connected`) →
//! `init.configure` (`init_configured`) → the manager's `hello` on port 1024 (`Ready`).
//! Teardown: `init.export` per rw workspace → staging → commit (`ws_sync_done`) →
//! `init.shutdown` → `SendCtrlAltDel` → SIGKILL (`vmm_exited`) → cleanup (`cleanup_done`).
//!
//! Host requirements: `/dev/kvm` read-write (group `kvm`), e2fsprogs ≥ 1.47.1 (or Docker /
//! fakeroot fallback) for images, Docker for rootfs builds, and for `unrestricted`
//! networking a one-time `sudo scripts/fc-net-setup.sh`. Not supported in V1:
//! `egress_restricted` (refused), published ports (`Unimplemented`: needs guest
//! `tunnel.open`), snapshots, live host-dir sharing.

pub mod api;
pub mod artifacts;
mod backend;
mod config;
pub mod control;
mod error;
pub mod hash;
pub mod net;
pub mod proc;
pub mod reap;
pub mod rootfs;
pub mod rundir;
mod vm;
pub mod vsock;
pub mod workspace;

pub use backend::{FirecrackerBackend, KVM_DEVICE, check_kvm};
pub use config::{FirecrackerConfig, RootfsSource};
pub use error::{Error, Result};
pub use net::{TapPlan, TapPoolConfig};
pub use reap::ReapReport;
pub use vm::GUEST_CID;
pub use vsock::{VsockConnector, vsock_connect};

use std::path::PathBuf;

/// Sandcrab state root: `$SANDCRAB_HOME`, else `$HOME/.sandcrab`.
pub fn sandcrab_home() -> Result<PathBuf> {
    if let Some(h) = std::env::var_os("SANDCRAB_HOME") {
        return Ok(PathBuf::from(h));
    }
    let home = std::env::var_os("HOME").ok_or(Error::NoHome)?;
    Ok(PathBuf::from(home).join(".sandcrab"))
}

/// Default cache root: `<sandcrab_home>/cache`.
pub fn default_cache_root() -> Result<PathBuf> {
    Ok(sandcrab_home()?.join("cache"))
}
