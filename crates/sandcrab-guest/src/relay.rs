//! `crabd relay`: a dumb TCP → Unix-socket byte pump.
//!
//! Used by the Docker backend's TCP transport (Docker Desktop on macOS, where a Unix socket
//! cannot cross the VM boundary). The sandbox container keeps its own network policy (for
//! `disabled`: no network at all) and serves on a Unix socket in a small shared tmpfs
//! volume; a separate, hardened relay container mounts that volume, listens on TCP and
//! Docker publishes the port on the host's `127.0.0.1` only.
//!
//! The relay interprets nothing: each accepted TCP connection gets a fresh Unix-socket
//! connection to `crabd` and bytes are copied both ways (half-close preserved) until either
//! side closes. Authentication is end to end: `crabd` itself demands the `auth` frame on
//! every connection, so the relay never sees or needs the token.

use std::{future::Future, io, net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

use tokio::{
    net::{TcpListener, TcpStream, UnixStream},
    sync::Semaphore,
    task::JoinSet,
};
use tracing::{debug, info, warn};

/// Relay settings.
#[derive(Debug, Clone)]
pub struct RelayConfig {
    /// TCP `host:port` to listen on.
    pub listen: String,
    /// `crabd`'s Unix socket.
    pub target: PathBuf,
    /// Max concurrently relayed connections; further clients wait in the accept backlog.
    pub max_connections: usize,
}

/// A bound relay.
pub struct Relay {
    listener: TcpListener,
    config: RelayConfig,
}

impl Relay {
    pub async fn bind(config: RelayConfig) -> io::Result<Self> {
        if config.max_connections == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "max_connections must be at least 1",
            ));
        }
        let listener = TcpListener::bind(&config.listen).await?;
        Ok(Self { listener, config })
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// Relay until `shutdown` resolves; open connections are then dropped.
    pub async fn run(self, shutdown: impl Future<Output = ()>) -> io::Result<()> {
        let Self { listener, config } = self;
        let target = Arc::new(config.target);
        let slots = Arc::new(Semaphore::new(config.max_connections));
        let mut conns = JoinSet::new();
        tokio::pin!(shutdown);
        info!(
            listen = %listener.local_addr()?,
            target = %target.display(),
            "crabd relay serving"
        );
        loop {
            while conns.try_join_next().is_some() {}
            let permit = tokio::select! {
                () = &mut shutdown => break,
                p = Arc::clone(&slots).acquire_owned() => p.expect("relay semaphore is never closed"),
            };
            let accepted = tokio::select! {
                () = &mut shutdown => break,
                a = listener.accept() => a,
            };
            let (tcp, peer) = match accepted {
                Ok(a) => a,
                Err(e) => {
                    warn!(error = %e, "accept failed");
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                }
            };
            let target = Arc::clone(&target);
            conns.spawn(async move {
                pump(tcp, &target, peer).await;
                drop(permit);
            });
        }
        info!(connections = conns.len(), "crabd relay stopped");
        Ok(())
    }
}

async fn pump(mut tcp: TcpStream, target: &std::path::Path, peer: SocketAddr) {
    let _ = tcp.set_nodelay(true);
    let mut unix = match UnixStream::connect(target).await {
        Ok(u) => u,
        Err(e) => {
            // crabd not up yet (the host retries) or gone.
            debug!(%peer, error = %e, "crabd socket unavailable; closing");
            return;
        }
    };
    match tokio::io::copy_bidirectional(&mut tcp, &mut unix).await {
        Ok((up, down)) => debug!(%peer, up, down, "relayed connection closed"),
        Err(e) => debug!(%peer, error = %e, "relayed connection ended"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::UnixListener;

    #[tokio::test]
    async fn relays_bytes_and_half_close() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("s.sock");
        let ul = UnixListener::bind(&sock).unwrap();
        // Echo server that also reports EOF by appending "!".
        tokio::spawn(async move {
            while let Ok((mut s, _)) = ul.accept().await {
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    s.read_to_end(&mut buf).await.unwrap();
                    buf.push(b'!');
                    s.write_all(&buf).await.unwrap();
                });
            }
        });
        let relay = Relay::bind(RelayConfig {
            listen: "127.0.0.1:0".into(),
            target: sock,
            max_connections: 4,
        })
        .await
        .unwrap();
        let addr = relay.local_addr().unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let h = tokio::spawn(relay.run(async {
            let _ = rx.await;
        }));
        for _ in 0..3 {
            let mut c = TcpStream::connect(addr).await.unwrap();
            c.write_all(b"ping").await.unwrap();
            c.shutdown().await.unwrap();
            let mut out = Vec::new();
            c.read_to_end(&mut out).await.unwrap();
            assert_eq!(out, b"ping!");
        }
        tx.send(()).unwrap();
        h.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn missing_target_closes_the_connection() {
        let dir = tempfile::tempdir().unwrap();
        let relay = Relay::bind(RelayConfig {
            listen: "127.0.0.1:0".into(),
            target: dir.path().join("nope.sock"),
            max_connections: 1,
        })
        .await
        .unwrap();
        let addr = relay.local_addr().unwrap();
        tokio::spawn(relay.run(std::future::pending()));
        let mut c = TcpStream::connect(addr).await.unwrap();
        let mut out = Vec::new();
        c.read_to_end(&mut out).await.unwrap();
        assert!(out.is_empty());
    }
}
