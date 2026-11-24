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
//!
//! [`PortForwards`] is what a microVM backend holds per sandbox: one forwarder per
//! `SandboxConfig.ports` entry (validated by [`validate_ports`]), reported as
//! [`PublishedPort`]s, and stopped on [`PortForwards::shutdown`], on drop, or as soon as a
//! backend-supplied "VM is gone" signal resolves (crash).

use std::{
    collections::BTreeSet,
    fmt,
    future::Future,
    net::{Ipv4Addr, SocketAddr},
    pin::Pin,
    sync::Arc,
    time::Duration,
};

use tokio::{
    net::{TcpListener, TcpStream},
    sync::{Semaphore, oneshot, watch},
    task::{JoinHandle, JoinSet},
};
use tracing::{debug, info, warn};

use crate::{Connector, PortSpec, PublishedPort, Result, SandboxError, client::open_tunnel};

/// Default max concurrent tunnels per forwarder.
pub const DEFAULT_MAX_TUNNELS: usize = 64;
/// Default budget for connect + `tunnel.open` handshake.
pub const DEFAULT_TUNNEL_OPEN_TIMEOUT: Duration = Duration::from_secs(10);
/// Relay buffer per direction.
pub const RELAY_BUF: usize = 64 * 1024;
/// Most guest ports one sandbox may publish (each is one host listener).
pub const MAX_PUBLISHED_PORTS: usize = 64;

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

/// Check a sandbox's published-port list: guest ports 1..=65535, no duplicates, at most
/// [`MAX_PUBLISHED_PORTS`]. Backends call this before creating anything.
pub fn validate_ports(ports: &[PortSpec]) -> Result<()> {
    if ports.len() > MAX_PUBLISHED_PORTS {
        return Err(SandboxError::InvalidConfig(format!(
            "{} published ports requested; at most {MAX_PUBLISHED_PORTS} per sandbox",
            ports.len()
        )));
    }
    let mut seen = BTreeSet::new();
    for p in ports {
        if p.guest == 0 {
            return Err(SandboxError::InvalidConfig(
                "guest port 0 is not publishable".into(),
            ));
        }
        if !seen.insert(p.guest) {
            return Err(SandboxError::InvalidConfig(format!(
                "guest port {} is listed twice",
                p.guest
            )));
        }
    }
    Ok(())
}

/// Resolves when the sandbox is gone (VMM exited): its forwards then stop on their own.
pub type StopSignal = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

/// Every published port of one sandbox: a [`PortForwarder`] per guest port, each on
/// `127.0.0.1:<ephemeral>`.
///
/// Stops (listeners closed, open tunnels cut) on [`shutdown`](Self::shutdown), on drop, or
/// when the `until` signal given to [`bind`](Self::bind) resolves. The reported
/// [`PublishedPort`]s do not change; after a stop their host ports refuse connections.
pub struct PortForwards {
    published: Vec<PublishedPort>,
    stop: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
}

impl fmt::Debug for PortForwards {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PortForwards")
            .field("published", &self.published)
            .finish_non_exhaustive()
    }
}

impl PortForwards {
    /// No published ports.
    #[must_use]
    pub fn none() -> Self {
        Self {
            published: Vec::new(),
            stop: None,
            task: None,
        }
    }

    /// Validate `ports` ([`validate_ports`]) and bind one forwarder per guest port on an
    /// ephemeral `127.0.0.1` port. A guest port nothing listens on yet is not an error: each
    /// connection to it is refused (`tunnel.open` fails with `-32004`, the client sees the
    /// connection close) until something listens. On a bind error every forwarder bound so
    /// far is closed again.
    pub async fn bind(
        connector: Arc<dyn Connector>,
        ports: &[PortSpec],
        config: ForwarderConfig,
        until: Option<StopSignal>,
    ) -> Result<Self> {
        validate_ports(ports)?;
        if ports.is_empty() {
            return Ok(Self::none());
        }
        let mut forwarders = Vec::with_capacity(ports.len());
        for p in ports {
            // Dropping `forwarders` on error closes the ones already bound.
            forwarders.push(PortForwarder::bind(Arc::clone(&connector), p.guest, 0, config).await?);
        }
        let published = forwarders
            .iter()
            .map(|f| PublishedPort {
                guest: f.guest_port(),
                host: f.local_addr(),
            })
            .collect();
        let (stop, stop_rx) = oneshot::channel::<()>();
        let until = until.unwrap_or_else(|| Box::pin(std::future::pending()));
        let task = tokio::spawn(async move {
            tokio::select! {
                _ = stop_rx => {}
                () = until => debug!("sandbox gone; closing its published ports"),
            }
            for f in forwarders {
                f.shutdown().await;
            }
        });
        Ok(Self {
            published,
            stop: Some(stop),
            task: Some(task),
        })
    }

    /// The published ports, in request order.
    #[must_use]
    pub fn published(&self) -> &[PublishedPort] {
        &self.published
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.published.is_empty()
    }

    /// Close every listener and tunnel, and wait for them to finish.
    pub async fn shutdown(mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(task) = self.task.take()
            && tokio::time::timeout(Duration::from_secs(10), task)
                .await
                .is_err()
        {
            warn!("published ports did not stop in time");
        }
    }
}

impl Drop for PortForwards {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            // Dropping the task's forwarders aborts their listeners and tunnels.
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

    fn ports(list: &[u16]) -> Vec<PortSpec> {
        list.iter().map(|&guest| PortSpec { guest }).collect()
    }

    async fn wait_closed(addr: SocketAddr, why: &str) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while TcpStream::connect(addr).await.is_ok() {
            assert!(
                std::time::Instant::now() < deadline,
                "{addr} still open: {why}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[test]
    fn port_list_validation() {
        validate_ports(&[]).unwrap();
        validate_ports(&ports(&[80, 8080, 65535])).unwrap();
        let err = validate_ports(&ports(&[0])).unwrap_err().to_string();
        assert!(err.contains("port 0"), "{err}");
        let err = validate_ports(&ports(&[80, 80])).unwrap_err().to_string();
        assert!(err.contains("twice"), "{err}");
        let many: Vec<u16> = (1..=u16::try_from(MAX_PUBLISHED_PORTS + 1).unwrap()).collect();
        assert!(matches!(
            validate_ports(&ports(&many)),
            Err(SandboxError::InvalidConfig(_))
        ));
    }

    #[tokio::test]
    async fn forwards_bind_report_and_stop() {
        let ep: Arc<dyn Connector> = Arc::new(Endpoint::Tcp("127.0.0.1:1".parse().unwrap()));
        let none = PortForwards::bind(Arc::clone(&ep), &[], ForwarderConfig::default(), None)
            .await
            .unwrap();
        assert!(none.is_empty());
        none.shutdown().await;

        let f = PortForwards::bind(
            Arc::clone(&ep),
            &ports(&[3000, 8080]),
            ForwarderConfig::default(),
            None,
        )
        .await
        .unwrap();
        let p = f.published().to_vec();
        assert_eq!(p.iter().map(|p| p.guest).collect::<Vec<_>>(), [3000, 8080]);
        for p in &p {
            assert_eq!(p.host.ip(), Ipv4Addr::LOCALHOST);
            assert_ne!(p.host.port(), 0);
            TcpStream::connect(p.host).await.unwrap();
        }
        f.shutdown().await;
        for p in &p {
            wait_closed(p.host, "after shutdown").await;
        }

        // The `until` signal stops them without a shutdown call.
        let (tx, rx) = oneshot::channel::<()>();
        let f = PortForwards::bind(
            Arc::clone(&ep),
            &ports(&[80]),
            ForwarderConfig::default(),
            Some(Box::pin(async {
                let _ = rx.await;
            })),
        )
        .await
        .unwrap();
        let addr = f.published()[0].host;
        TcpStream::connect(addr).await.unwrap();
        tx.send(()).unwrap();
        wait_closed(addr, "after the until signal").await;
        drop(f);

        // So does dropping them.
        let f = PortForwards::bind(ep, &ports(&[81]), ForwarderConfig::default(), None)
            .await
            .unwrap();
        let addr = f.published()[0].host;
        drop(f);
        wait_closed(addr, "after drop").await;
    }

    /// Through the mock `crabd`'s `tunnel.open`: a listening "guest" port round-trips with
    /// half-close; a port nothing listens on refuses only that connection.
    #[tokio::test]
    async fn forwards_through_mock_crabd() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        use crate::testing::{MockConnector, MockCrabd};

        let echo = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let echo_port = echo.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((s, _)) = echo.accept().await {
                tokio::spawn(async move {
                    let (mut r, mut w) = s.into_split();
                    let _ = tokio::io::copy(&mut r, &mut w).await;
                    let _ = w.shutdown().await;
                });
            }
        });
        let closed = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let closed_port = closed.local_addr().unwrap().port();
        drop(closed);

        let mock = MockCrabd::default();
        let conn: Arc<dyn Connector> = Arc::new(MockConnector::new(mock.clone()));
        let f = PortForwards::bind(
            conn,
            &ports(&[echo_port, closed_port]),
            ForwarderConfig::default(),
            None,
        )
        .await
        .unwrap();
        let (open, refused) = (f.published()[0].host, f.published()[1].host);

        let mut s = TcpStream::connect(open).await.unwrap();
        s.write_all(b"ping").await.unwrap();
        s.shutdown().await.unwrap();
        let mut back = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), s.read_to_end(&mut back))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(back, b"ping");
        assert_eq!(mock.tunnels.load(std::sync::atomic::Ordering::SeqCst), 1);

        let mut s = TcpStream::connect(refused).await.unwrap();
        let mut buf = Vec::new();
        let n = tokio::time::timeout(Duration::from_secs(5), s.read_to_end(&mut buf))
            .await
            .unwrap()
            .unwrap_or(0);
        assert_eq!(n, 0, "refused tunnel must just close");
        assert_eq!(mock.tunnels.load(std::sync::atomic::Ordering::SeqCst), 1);
        // The forwarder survives the refusal.
        let mut s = TcpStream::connect(open).await.unwrap();
        s.write_all(b"again").await.unwrap();
        s.shutdown().await.unwrap();
        let mut back = Vec::new();
        s.read_to_end(&mut back).await.unwrap();
        assert_eq!(back, b"again");
        f.shutdown().await;
    }

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
