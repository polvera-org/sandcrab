//! OCI image → ext4 rootfs pipeline shared by Sandcrab's microVM backends (Firecracker, libkrun).
//!
//! ```text
//! sandcrab/<tpl>:0.1.0 ─docker create+export─▶ tar ─augment─▶ rootfs.tar ─mke2fs -d─▶ rootfs.ext4 (ro, shared)
//! ```
//!
//! Pure std + `tar`/`sha2`, no VMM code. It drives the `docker` and `mke2fs` (or `fakeroot`)
//! binaries.
//!
//! Modules:
//! - [`cache`] (re-exported at the root): [`RootfsBuilder`], cache keys, manifests, scratch drives.
//! - [`arch`]: [`Arch`], the guest platform (`linux/amd64` | `linux/arm64`) a rootfs targets.
//! - [`augment`]: streaming tar rewriter that injects `crabd` and the guest mountpoints.
//! - [`mkfs`]: rootless `mke2fs` strategies (native tar, helper container, fakeroot).
//! - [`hash`]: SHA-256 helpers.
//! - [`stamp`]: stat stamps, so a pinned file is verified with one `fstat` and used through
//!   the verified fd.
//! - [`error`]: the [`Error`] type, shared with the backends' artifact plumbing.

pub mod arch;
pub mod augment;
pub mod cache;
pub mod error;
pub mod hash;
pub mod mkfs;
pub mod stamp;

pub use arch::Arch;
pub use cache::*;
pub use error::{Error, IoCtx, Result};
pub use mkfs::MkfsStrategy;
pub use stamp::{FileStamp, Sha256Digest};

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
