//! Firecracker microVM backend for Sandcrab.
//!
//! This crate currently ships only the host-side **artifact plumbing** the backend needs:
//!
//! - [`rootfs`]: OCI image → read-only ext4 rootfs pipeline (`docker export` → tar augmentation with
//!   `crabd` as `/sbin/crabd` → rootless `mke2fs -d`), a content-addressed cache, and per-sandbox
//!   sparse scratch drives.
//! - [`artifacts`]: pinned guest kernel and Firecracker release binaries, fetched once and verified by
//!   SHA-256.
//!
//! The `RuntimeBackend` implementation itself (API client, vsock transport, VM lifecycle,
//! `BackendCapabilities`) lands in flow step `fc-backend`; see `docs/design/firecracker-backend.md`.

pub mod artifacts;
mod error;
pub mod hash;
pub mod rootfs;

pub use error::{Error, Result};

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
