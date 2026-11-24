//! The artifact-plumbing error type lives in `sandcrab-rootfs` (shared with the rootfs pipeline);
//! re-exported here so `sandcrab_backend_firecracker::{Error, Result}` keep their paths.

pub use sandcrab_rootfs::error::{Error, Result};

pub(crate) use sandcrab_rootfs::error::IoCtx;
