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
//! `/opt/sandcrab/crabd`. The host talks the real Sandcrab protocol to it — the same
//! `crabd` and wire format the microVM backends use. There is no `docker exec` anywhere.
//!
//! Transports ([`DockerTransport`], `$SANDCRAB_DOCKER_TRANSPORT=unix|tcp`):
//! - `unix` (default on Linux): `crabd` listens on `/run/sandcrab/crabd.sock`, which is the
//!   per-sandbox host directory `<state_dir>/<sandbox id>/` (0700).
//! - `tcp` (default on macOS / Docker Desktop, where a Unix socket cannot cross the VM
//!   boundary): `crabd` listens on a Unix socket in a per-sandbox tmpfs volume; a second,
//!   hardened relay container (`crabd relay`) publishes it on host `127.0.0.1` (ephemeral
//!   port, never `0.0.0.0`). The sandbox container's network is untouched, so policy
//!   `disabled` still means network mode `none`.
//!
//! Either way every connection must first present a per-sandbox 256-bit token
//! (`sandcrab_protocol::auth`): the host writes it to `<state_dir>/<id>/auth-token` (0400),
//! `crabd` reads and deletes it, and the host keeps it only in memory. A loopback TCP port is
//! reachable by every local user and process, so the token, not the port, is the boundary.
//!
//! Requirements / limitations:
//! - Template images must be pinned on this host (`templates/build.sh <name>` or
//!   `sandcrab template pin <name>`; see `sandcrab_core::pins`) unless
//!   [`DockerConfig::allow_unpinned_images`] is set. The pin is local integrity, not provenance.
//! - A local Docker daemon that can bind-mount host paths: rootful Docker on Linux, or
//!   Docker Desktop on macOS (`tcp` transport). Rootless Docker remaps uids, so the
//!   workspace ownership model breaks; Docker Desktop for Linux needs
//!   `SANDCRAB_DOCKER_TRANSPORT=tcp`.
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
    CRABD_ENV, DockerConfig, DockerLimits, DockerTransport, TRANSPORT_ENV, check_static_crabd,
    default_crabd_path, default_state_dir,
};
pub use reap::{Owner, ReapReport, ReapScope, ReapedContainer, boot_id, state_dir_key};

/// Pre-rename name of [`DockerBackend`]; kept for one release.
#[deprecated(note = "renamed to `DockerBackend`")]
pub type DockerDevBackend = DockerBackend;

/// Pre-rename name of [`DockerConfig`]; kept for one release.
#[deprecated(note = "renamed to `DockerConfig`")]
pub type DockerDevConfig = DockerConfig;
