//! Host-side port forwarding over `crabd` tunnels: the microVM replacement for Docker's
//! published ports.
//!
//! A [`PortForwarder`] listens on **`127.0.0.1`** only (never a wildcard address; the IP is
//! not configurable). For every accepted TCP connection it opens a fresh transport
//! connection through the sandbox's [`Connector`] (vsock `CONNECT` on Firecracker, the
//! vsock UDS on libkrun, the Unix socket on Docker), sends `tunnel.open{port}`
//! ([`open_tunnel`]) and relays bytes 1:1 both ways with fixed buffers. Half-close is
//! propagated; either side closing or erroring ends the tunnel.
//!
//! Limits: at most [`ForwarderConfig::max_tunnels`] tunnels at once per forwarder (excess
//! connections are accepted and closed at once, never queued); `crabd` applies its own
//! per-sandbox cap on top. [`PortForwarder::shutdown`] (or dropping the forwarder) closes the
//! listener and every open tunnel, so a forwarder must not outlive its sandbox.

use std::{
    fmt,
    net::{Ipv4Addr, SocketAddr},
    sync::Arc,
    time::Duration,
};

use tokio::{
    net::{TcpListener, TcpStream},
    sync::{Semaphore, watch},
    task::{JoinHandle, JoinSet},
};
use tracing::{debug, info, warn};

use crate::{Connector, Result, SandboxError, client::open_tunnel};

/// Default max concurrent tunnels per forwarder.
pub const DEFAULT_MAX_TUNNELS: usize = 64;
/// Default budget for connect + `tunnel.open` handshake.
pub const DEFAULT_TUNNEL_OPEN_TIMEOUT: Duration = Duration::from_secs(10);
/// Relay buffer per direction.
pub const RELAY_BUF: usize = 64 * 1024;

/// Forwarder limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ForwarderConfig {
    pub max_tunnels: usize,
    pub open_timeout: Duration,
}

impl Default for ForwarderConfig {
    fn default() -> Self {
        Self {
            max_tunnels: DEFAULT_MAX_TUNNELS,
            open_timeout: DEFAULT_TUNNEL_OPEN_TIMEOUT,
        }
    }
}

/// A `127.0.0.1:<host_port>` → guest `127.0.0.1:<guest_port>` forward. Stops on
/// [`shutdown`](Self::shutdown) or drop.
pub struct PortForwarder {
    local_addr: SocketAddr,
    guest_port: u16,
    slots: Arc<Semaphore>,
    max_tunnels: usize,
    stop: watch::Sender<bool>,
    task: Option<JoinHandle<()>>,
}

impl fmt::Debug for PortForwarder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PortForwarder")
            .field("local_addr", &self.local_addr)
            .field("guest_port", &self.guest_port)
            .field("active_tunnels", &self.active_tunnels())
            .finish_non_exhaustive()
    }
}

impl PortForwarder {
    /// Bind `127.0.0.1:<host_port>` (`0` = ephemeral) and start forwarding to guest loopback
    /// `guest_port` through `connector`.
    pub async fn bind(
        connector: Arc<dyn Connector>,
        guest_port: u16,
        host_port: u16,
        config: ForwarderConfig,
    ) -> Result<Self> {
        if guest_port == 0 {
            return Err(SandboxError::InvalidConfig(
                "guest port must be between 1 and 65535".into(),
            ));
        }
        if config.max_tunnels == 0 {
            return Err(SandboxError::InvalidConfig(
                "max_tunnels must be at least 1".into(),
            ));
        }
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, host_port))
            .await
            .map_err(|e| {
                SandboxError::InvalidConfig(format!("cannot bind 127.0.0.1:{host_port}: {e}"))
            })?;
        let local_addr = listener
            .local_addr()
            .map_err(|e| SandboxError::Transport(format!("listener address: {e}")))?;
        let slots = Arc::new(Semaphore::new(config.max_tunnels));
        let (stop, stop_rx) = watch::channel(false);
        let task = tokio::spawn(accept_loop(
            listener,
            connector,
            guest_port,
            config,
            Arc::clone(&slots),
            stop_rx,
        ));
        info!(%local_addr, guest_port, "port forward listening");
        Ok(Self {
            local_addr,
            guest_port,
            slots,
            max_tunnels: config.max_tunnels,
            stop,
            task: Some(task),
        })
    }

    /// The bound host address (always `127.0.0.1:<port>`).
    #[must_use]
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    #[must_use]
    pub fn guest_port(&self) -> u16 {
        self.guest_port
    }

    /// Tunnels currently open (or opening).
    #[must_use]
    pub fn active_tunnels(&self) -> usize {
        self.max_tunnels - self.slots.available_permits()
    }

    /// Stop accepting, close every open tunnel and wait for them to finish.
    pub async fn shutdown(mut self) {
        let _ = self.stop.send(true);
        if let Some(task) = self.task.take()
            && tokio::time::timeout(Duration::from_secs(5), task)
                .await
                .is_err()
        {
            warn!(local_addr = %self.local_addr, "port forward did not stop in time");
        }
    }
}

impl Drop for PortForwarder {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
        if let Some(task) = self.task.take() {
            // Dropping the accept loop's JoinSet aborts every tunnel task.
            task.abort();
        }
    }
}

async fn accept_loop(
    listener: TcpListener,
    connector: Arc<dyn Connector>,
    guest_port: u16,
    config: ForwarderConfig,
    slots: Arc<Semaphore>,
    mut stop: watch::Receiver<bool>,
) {
    let mut tunnels = JoinSet::new();
    loop {
        while tunnels.try_join_next().is_some() {}
        let accepted = tokio::select! {
            biased;
            _ = stop.wait_for(|s| *s) => break,
            a = listener.accept() => a,
        };
        let (client, peer) = match accepted {
            Ok(c) => c,
            Err(e) => {
                warn!(error = %e, "port forward accept failed");
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }
        };
        let Ok(slot) = Arc::clone(&slots).try_acquire_owned() else {
            warn!(%peer, guest_port, limit = config.max_tunnels, "tunnel limit reached; closing connection");
            drop(client);
            continue;
        };
        let connector = Arc::clone(&connector);
        let stop = stop.clone();
        tunnels.spawn(async move {
            relay(client, peer, connector.as_ref(), guest_port, config, stop).await;
            drop(slot);
        });
    }
    drop(listener);
    // Tunnels watch `stop` too; give them a moment to unwind, then abort the rest.
    let drained = tokio::time::timeout(Duration::from_secs(2), async {
        while tunnels.join_next().await.is_some() {}
    })
    .await;
    if drained.is_err() {
        tunnels.abort_all();
        while tunnels.join_next().await.is_some() {}
    }
    debug!(guest_port, "port forward stopped");
}

async fn relay(
    mut client: TcpStream,
    peer: SocketAddr,
    connector: &dyn Connector,
    guest_port: u16,
    config: ForwarderConfig,
    mut stop: watch::Receiver<bool>,
) {
    let _ = client.set_nodelay(true);
    let opened = tokio::select! {
        biased;
        _ = stop.wait_for(|s| *s) => return,
        t = open_tunnel(connector, guest_port, config.open_timeout) => t,
    };
    let mut tunnel = match opened {
        Ok(t) => t,
        Err(e) => {
            // The client sees the connection close: the closest TCP analogue of a refusal.
            debug!(%peer, guest_port, error = %e, "tunnel.open failed");
            return;
        }
    };
    let copy =
        tokio::io::copy_bidirectional_with_sizes(&mut client, &mut tunnel, RELAY_BUF, RELAY_BUF);
    tokio::select! {
        biased;
        _ = stop.wait_for(|s| *s) => debug!(%peer, guest_port, "tunnel closed: forwarder stopped"),
        r = copy => match r {
            Ok((up, down)) => debug!(%peer, guest_port, host_to_guest = up, guest_to_host = down, "tunnel closed"),
            Err(e) => debug!(%peer, guest_port, error = %e, "tunnel ended with error"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Endpoint;

    #[tokio::test]
    async fn binds_loopback_only_and_validates() {
        let ep: Arc<dyn Connector> = Arc::new(Endpoint::Tcp("127.0.0.1:1".parse().unwrap()));
        let f = PortForwarder::bind(Arc::clone(&ep), 80, 0, ForwarderConfig::default())
            .await
            .unwrap();
        assert!(f.local_addr().ip().is_loopback());
        assert_ne!(f.local_addr().port(), 0);
        assert_eq!(f.active_tunnels(), 0);
        f.shutdown().await;

        assert!(matches!(
            PortForwarder::bind(Arc::clone(&ep), 0, 0, ForwarderConfig::default()).await,
            Err(SandboxError::InvalidConfig(_))
        ));
        let cfg = ForwarderConfig {
            max_tunnels: 0,
            ..ForwarderConfig::default()
        };
        assert!(matches!(
            PortForwarder::bind(ep, 80, 0, cfg).await,
            Err(SandboxError::InvalidConfig(_))
        ));
    }
}
