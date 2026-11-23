//! Docker/OCI local development and CI backend for Sandcrab (`docker-dev`).
//!
//! **Security note**: containers share the host kernel. This backend reports
//! `hardware_isolated = false` and `multi_tenant_safe = false`, and
//! [`DockerDevBackend::new`] refuses to construct it unless
//! [`DockerDevConfig::allow_insecure_dev_backend`] is explicitly set (default `false`).
//! It is never a fallback for a microVM backend.
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
//!   [`DockerDevConfig::allow_unpinned_images`] is set.
//! - Linux host with a local, rootful Docker daemon (bind mounts use host paths; Unix
//!   sockets do not cross Docker Desktop's VM file sharing, and rootless Docker remaps
//!   uids so the socket / workspace ownership model breaks).
//! - `egress_restricted` networking is refused (`PolicyUnenforceable`), never downgraded.
//! - Template `disk_mb` is **not enforced** ([`DockerDevBackend::enforces_disk_limit`] is
//!   `false`; every spawn logs a warning); the container root filesystem is writable.
//! - A spawn future cancelled mid-create can leak a labeled container; see
//!   [`DockerDevBackend::reap_orphans`].

mod backend;
mod config;
mod reap;
pub mod spec;

pub use backend::{DISK_LIMIT_WARN_TARGET, DockerDevBackend, DockerImageVerifier};
pub use config::{
    CRABD_ENV, DockerDevConfig, DockerLimits, check_static_crabd, default_crabd_path,
    default_state_dir,
};
pub use reap::{Owner, ReapReport, ReapScope, ReapedContainer, boot_id, state_dir_key};
