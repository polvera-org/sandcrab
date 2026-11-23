//! `crabd` — the Sandcrab guest daemon, as a library.
//!
//! `crabd` runs inside every sandbox (container or microVM) and answers framed JSON-RPC 2.0
//! requests from the host (see [`sandcrab_protocol`]). The binary (`src/main.rs`) is a thin
//! CLI over this crate so the server can also be driven in-process by tests.
//!
//! Layout:
//! - [`codec`] — async adapter over the protocol crate's synchronous frame helpers.
//! - [`server`] — listeners (UDS / TCP; vsock later), per-connection request multiplexing,
//!   `hello` / `ping`, graceful shutdown.
//! - [`context`] — [`ToolContext`] shared by every tool call (default cwd, limits, processes).
//! - [`tools`] — [`tools::dispatch`] plus one module per tool family.

pub mod context;
pub mod tools;

pub use context::{Limits, ToolContext};

/// `crabd` build version reported by `hello`.
pub const CRABD_VERSION: &str = env!("CARGO_PKG_VERSION");
