//! Forwards: a loopback port inside the sandbox whose connections go to a target chosen on
//! the host side, one decision per connection. The sandbox keeps its network policy
//! (`disabled` stays disabled) and nothing is published on the host.
//!
//! Ask for the ports at spawn ([`crate::SandboxConfig::forward`]), then give the sandbox its
//! target once: [`crate::Sandbox::serve_forwards`] with a [`ForwardTarget`], or
//! [`crate::Sandbox::serve_forwards_with`] with a closure. For each connection a program
//! makes to a forward port the target gets the port and answers with a byte stream (accept:
//! the two are relayed, half-close included) or an error (reject: the program's connection
//! is closed). Until a target is given, and whenever the host side is not listening, the
//! guest refuses connections; [`crate::Sandbox::forward_stats`] counts all of it.
//!
//! A backend that cannot forward refuses the spawn with
//! [`crate::SandboxError::PolicyUnenforceable`]; it never starts a sandbox whose forward
//! port leads nowhere.
//!
//! ```no_run
//! use std::io;
//! use sandcrab_sdk::{SandboxConfig, Sandcrab};
//! use tokio::net::TcpStream;
//!
//! # async fn demo(sandcrab: &Sandcrab) -> sandcrab_sdk::Result<()> {
//! // Programs in the sandbox reach a database of the host at 127.0.0.1:5432, and
//! // nothing else.
//! let sandbox = sandcrab
//!     .spawn(SandboxConfig::from_template("base").forward(5432))
//!     .await?;
//! sandbox.serve_forwards_with(|guest_port| async move {
//!     match guest_port {
//!         5432 => TcpStream::connect(("127.0.0.1", 5432)).await,
//!         _ => Err(io::Error::other("not a port this target serves")),
//!     }
//! })?;
//!
//! for port in sandbox.forward_stats().await? {
//!     println!("{}: {} accepted, {:?}", port.port, port.accepted, port.refused);
//! }
//! # Ok(())
//! # }
//! ```

use std::future::Future;
use std::io;

use async_trait::async_trait;
use sandcrab_core::{AsyncStream, BoxedStream, ForwardTarget, SandboxId};

/// A closure as a [`ForwardTarget`].
pub(crate) struct FnTarget<F>(pub(crate) F);

#[async_trait]
impl<F, Fut, S> ForwardTarget for FnTarget<F>
where
    F: Fn(u16) -> Fut + Send + Sync,
    Fut: Future<Output = io::Result<S>> + Send,
    S: AsyncStream + 'static,
{
    async fn connect(&self, _sandbox: SandboxId, guest_port: u16) -> io::Result<BoxedStream> {
        Ok(Box::new((self.0)(guest_port).await?))
    }
}

#[cfg(test)]
mod tests;
