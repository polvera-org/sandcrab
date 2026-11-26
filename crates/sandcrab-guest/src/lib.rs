//! `crabd` — the Sandcrab guest daemon, as a library.
//!
//! `crabd` runs inside every sandbox (container or microVM) and answers framed JSON-RPC 2.0
//! requests from the host (see [`sandcrab_protocol`]). The binary (`src/main.rs`) is a thin
//! CLI over this crate so the server can also be driven in-process by tests.
//!
//! Layout:
//! - [`auth`] — the `auth` frame every connection must open with when a token is set.
//! - [`preauth`] — the pre-authentication budget (evict-oldest queue, short deadline).
//! - [`relay`] — `crabd relay`, the TCP → Unix-socket pump of the Docker TCP transport.
//! - [`codec`] — async adapter over the protocol crate's synchronous frame helpers.
//! - [`server`] — listeners (UDS / TCP / vsock), per-connection request multiplexing,
//!   `hello` / `ping`, graceful shutdown.
//! - [`context`] — [`ToolContext`] shared by every tool call (default cwd, limits, processes).
//! - [`hardening`] — `no_new_privs` for `crabd serve` and everything it spawns.
//! - [`tools`] — [`tools::dispatch`] plus one module per tool family.
//! - [`tunnel`] — `tunnel.open`: raw relay to a guest loopback TCP port.
//! - [`forward`] — `forward.listen` / `forward.accept` / `forward.stats`: guest loopback ports
//!   whose connections the host routes.
//! - [`supervise`] — `process.open` / `process.cancel` and `crabd supervise`, the per-program
//!   supervisor that owns the connection and stops the program's whole tree.
//! - [`proctree`] — the process tree of a supervisor, read from `/proc`.
//! - [`probe`] — `crabd probe` (the self-test's in-sandbox half; not implemented yet).
//! - [`vsock`] — AF_VSOCK listener (Linux only).
//! - [`init`] — `crabd init`, PID 1 of a microVM (control port, mounts, workspace export).

pub mod auth;
pub mod codec;
pub mod context;
pub mod forward;
pub mod hardening;
pub mod init;
pub mod preauth;
pub mod probe;
pub mod proctree;
pub mod relay;
pub mod server;
pub mod supervise;
pub mod tools;
pub mod tunnel;
pub mod vsock;

pub use auth::AuthConfig;
pub use context::{Limits, ToolContext};
pub use server::{ListenAddr, Server, ServerConfig};
pub use tunnel::TunnelConfig;

/// `crabd` build version reported by `hello`.
pub const CRABD_VERSION: &str = env!("CARGO_PKG_VERSION");
