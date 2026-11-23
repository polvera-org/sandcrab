//! Docker/OCI backend for Sandcrab (`docker`): an officially supported backend with fast
//! spawn / teardown, for single-tenant or trusted-host workloads.
//!
//! **Security note**: containers are hardened but share the host kernel. This backend
//! reports `hardware_isolated = false` and `multi_tenant_safe = false`; mutually hostile
//! multi-tenant code needs a microVM backend. It must be selected explicitly and is never
//! a fallback for a microVM backend.
//!
//! Each sandbox is one hardened container (see [`spec`] for the exact hardening) running
//! the template image with the host's **static** `crabd` bind-mounted read-only at
//! `/opt/sandcrab/crabd`. `crabd` listens on `/run/sandcrab/crabd.sock`, which is a
//! per-sandbox host directory `<state_dir>/<sandbox id>/` (0700), so the host talks the
//! real Sandcrab protocol over a Unix socket — the same `crabd` and wire format the
//! microVM backends use. There is no `docker exec` anywhere.
//!
//! Requirements / limitations:
//! - Templates must be pinned (`sha256` in `sandcrab.toml`) unless
//!   [`DockerConfig::allow_unpinned_images`] is set.
//! - Linux host with a local, rootful Docker daemon (bind mounts use host paths; Unix
//!   sockets do not cross Docker Desktop's VM file sharing, and rootless Docker remaps
//!   uids so the socket / workspace ownership model breaks).
//! - `egress_restricted` networking is refused (`PolicyUnenforceable`), never downgraded.
//! - Template `disk_mb` is **not enforced** ([`DockerBackend::enforces_disk_limit`] is
//!   `false`; every spawn logs a warning); the container root filesystem is writable.
//! - A spawn future cancelled mid-create can leak a labeled container; see
//!   [`DockerBackend::reap_orphans`].

mod backend;
mod config;
mod reap;
pub mod spec;

pub use backend::{DISK_LIMIT_WARN_TARGET, DockerBackend, DockerImageVerifier};
pub use config::{
    CRABD_ENV, DockerConfig, DockerLimits, check_static_crabd, default_crabd_path,
    default_state_dir,
};
pub use reap::{Owner, ReapReport, ReapScope, ReapedContainer, boot_id, state_dir_key};

/// Pre-rename name of [`DockerBackend`]; kept for one release.
#[deprecated(note = "renamed to `DockerBackend`")]
pub type DockerDevBackend = DockerBackend;

/// Pre-rename name of [`DockerConfig`]; kept for one release.
#[deprecated(note = "renamed to `DockerConfig`")]
pub type DockerDevConfig = DockerConfig;
