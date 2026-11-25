//! libkrun microVM backend for Sandcrab (design: `docs/design/libkrun-backend.md`).
//!
//! A hardware-isolated [`sandcrab_core::RuntimeBackend`] on **Apple Silicon macOS**
//! (Hypervisor.framework) and **Linux KVM**, with the same `crabd`, protocol and guest init as
//! the Firecracker backend. One signed helper process (`sandcrab-krun`) per sandbox reads a
//! [`HelperConfig`] from stdin and *becomes* the VMM; the backend never calls libkrun
//! in-process. libkrun is loaded at runtime from a pinned path, so this crate builds and
//! tests anywhere without libkrun installed.
//!
//! Capabilities: `hardware_isolated: true, multi_tenant_safe: false, supports_snapshots: false,
//! supports_host_dir_mounts: true` (live virtio-fs). TSI networking is never enabled
//! implicitly (design §9).
//!
//! Modules:
//! - [`config`] — [`LibkrunConfig`].
//! - [`helper_proto`] — the backend → helper wire format ([`HelperConfig`]) and its validation,
//!   plus helper exit codes.
//! - [`ffi`] — runtime-loaded libkrun table + safe wrapper (the only `unsafe`; used by the helper).
//! - [`artifacts`] — libkrun / libkrunfw pins and resolution.
//! - [`netstack`] — Phase B `unrestricted` networking: pinned passt / gvproxy behind the
//!   helper's egress filter.
//! - [`verified`] — pinned files opened once, checked by stat stamp / SHA-256 and used
//!   through that fd; the helper's install record.
//! - [`control`], [`workspace`], [`net`], [`reap`], [`ports`]: backend internals, see each module.

// Module layout (design §15). Each later flow step owns its own file(s), so they can land in
// parallel without touching this one.
pub mod artifacts;
mod backend;
#[cfg(target_os = "linux")]
mod cgroup;
pub mod config;
pub mod control;
#[allow(unsafe_code)]
pub mod ffi;
pub mod helper_proto;
pub mod net;
pub mod netstack;
pub mod netstack_pins;
pub mod ports;
pub mod reap;
pub mod verified;
mod vm;
pub mod workspace;

pub use backend::{CAPABILITIES, KrunLibs, LibkrunBackend, NetstackBin};
/// Linux: validate a delegated cgroup v2 parent for the helper leaves (`doctor`).
#[cfg(target_os = "linux")]
pub use cgroup::check_parent as check_cgroup_parent;
pub use config::{LibkrunConfig, RootfsSource};
pub use helper_proto::{
    HelperConfig, HelperConfigError, HelperNetwork, NetstackSpec, VirtiofsSemantics,
};
