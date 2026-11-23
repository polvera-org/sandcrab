//! `crabd` — the Sandcrab guest daemon, as a library.
//!
//! `crabd` runs inside every sandbox (container or microVM) and answers framed JSON-RPC 2.0
//! requests from the host (see [`sandcrab_protocol`]). The binary (`src/main.rs`) is a thin
//! CLI over this crate so the server can also be driven in-process by tests.
//!
//! Layout:
//! - [`codec`] — async adapter over the protocol crate's synchronous frame helpers.
//! - [`server`] — listeners (UDS / TCP / vsock), per-connection request multiplexing,
//!   `hello` / `ping`, graceful shutdown.
//! - [`context`] — [`ToolContext`] shared by every tool call (default cwd, limits, processes).
//! - [`tools`] — [`tools::dispatch`] plus one module per tool family.
//! - [`vsock`] — AF_VSOCK listener (Linux only).
//! - [`init`] — `crabd init`, PID 1 of a microVM (control port, mounts, workspace export).

pub mod codec;
pub mod context;
pub mod server;
pub mod tools;
pub mod vsock;

pub use context::{Limits, ToolContext};
pub use server::{ListenAddr, Server, ServerConfig};

/// `crabd` build version reported by `hello`.
pub const CRABD_VERSION: &str = env!("CARGO_PKG_VERSION");
