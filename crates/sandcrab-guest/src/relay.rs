//! `crabd relay`: a TCP → Unix-socket byte pump.
//!
//! Used by the Docker backend's TCP transport (Docker Desktop on macOS, where a Unix socket
//! cannot cross the VM boundary). The sandbox container keeps its own network policy (for
//! `disabled`: no network at all) and serves on a Unix socket in a small shared tmpfs
//! volume; a separate, hardened relay container mounts that volume, listens on TCP and
//! Docker publishes the port on the host's `127.0.0.1` only.
//!
//! Each accepted TCP connection gets a fresh Unix-socket connection to `crabd` and bytes are
//! copied both ways (half-close preserved) until either side closes. Authentication is end to
//! end: `crabd` itself demands the `auth` frame on every connection, so the relay never sees
//! or needs the token.
//!
//! The relay reads exactly one thing: `crabd`'s **first** frame on each connection (its
//! verdict on the `auth` frame). Until that arrives the connection is *pending*: it sits in a
//! small [`PendingQueue`] with a short deadline (oldest evicted first), not in one of the
//! [`RelayConfig::max_connections`] slots. Only a connection whose first `crabd` frame is a
//! success takes a slot; anything else (`UNAUTHORIZED`, garbage, EOF, deadline) is forwarded
//! and the connection is closed at once. So unauthenticated peers can neither starve the
//! authenticated client of relay slots nor hold a relay connection open after `crabd` has
//! refused it.

use std::{future::Future, io, net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

use sandcrab_protocol::{HEADER_LEN, ResponsePayload, RpcMessage};
use tokio::{
    io::AsyncWriteExt,
    net::{TcpListener, TcpStream, UnixStream},
    sync::Semaphore,
    task::JoinSet,
};
use tracing::{debug, info, warn};

use crate::{
    codec,
    preauth::{PendingQueue, PendingTicket, RateLimitedLog},
};

/// Default deadline for `crabd`'s first frame: its own auth deadline plus a margin.
pub const DEFAULT_PENDING_TIMEOUT: Duration = Duration::from_secs(4);

/// Relay settings.
#[derive(Debug, Clone)]
pub struct RelayConfig {
    /// TCP `host:port` to listen on.
    pub listen: String,
    /// `crabd`'s Unix socket.
    pub target: PathBuf,
    /// Max concurrently relayed (authenticated) connections; further ones are closed.
    pub max_connections: usize,
    /// Max connections waiting for `crabd`'s first frame (the oldest is evicted when full).
    pub max_pending: usize,
    /// How long a connection may wait for `crabd`'s first frame.
    pub pending_timeout: Duration,
}

impl RelayConfig {
    #[must_use]
    pub fn new(listen: impl Into<String>, target: impl Into<PathBuf>) -> Self {
        Self {
            listen: listen.into(),
            target: target.into(),
            max_connections: 256,
            max_pending: crate::preauth::DEFAULT_PENDING,
            pending_timeout: DEFAULT_PENDING_TIMEOUT,
        }
    }
}

/// A bound relay.
pub struct Relay {
    listener: TcpListener,
    config: RelayConfig,
}

impl Relay {
    pub async fn bind(config: RelayConfig) -> io::Result<Self> {
        if config.max_connections == 0 || config.max_pending == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "max_connections and max_pending must be at least 1",
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
        let shared = Arc::new(Shared {
            target: config.target,
            slots: Arc::new(Semaphore::new(config.max_connections)),
            pending_timeout: config.pending_timeout,
            refusals: RateLimitedLog::new(Duration::from_secs(1)),
        });
        let pending = PendingQueue::new(config.max_pending);
        let mut conns = JoinSet::new();
        tokio::pin!(shutdown);
        info!(
            listen = %listener.local_addr()?,
            target = %shared.target.display(),
            max_connections = config.max_connections,
            max_pending = config.max_pending,
            "crabd relay serving"
        );
        loop {
            while conns.try_join_next().is_some() {}
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
            let (ticket, evicted) = pending.admit();
            if evicted {
                shared.refusals.warn(
                    &peer.to_string(),
                    "pending connections full; evicted the oldest unauthenticated one",
                );
            }
            let shared = Arc::clone(&shared);
            conns.spawn(async move { pump(tcp, peer, ticket, &shared).await });
        }
        info!(connections = conns.len(), "crabd relay stopped");
        Ok(())
    }
}

struct Shared {
    target: PathBuf,
    slots: Arc<Semaphore>,
    pending_timeout: Duration,
    refusals: RateLimitedLog,
}

/// Whether `crabd`'s first frame accepts the connection (a success response).
fn accepted(payload: &[u8]) -> bool {
    matches!(
        RpcMessage::from_slice(payload),
        Ok(RpcMessage::Response(r)) if matches!(r.payload, ResponsePayload::Result(_))
    )
}

async fn pump(tcp: TcpStream, peer: SocketAddr, mut ticket: PendingTicket, shared: &Shared) {
    let _ = tcp.set_nodelay(true);
    let unix = match ticket
        .run(shared.pending_timeout, UnixStream::connect(&shared.target))
        .await
    {
        Ok(Ok(u)) => u,
        Ok(Err(e)) => {
            // crabd not up yet (the host retries) or gone.
            debug!(%peer, error = %e, "crabd socket unavailable; closing");
            return;
        }
        Err(end) => {
            debug!(%peer, ?end, "closed before crabd answered");
            return;
        }
    };
    let (mut tcp_r, mut tcp_w) = tcp.into_split();
    let (mut unix_r, mut unix_w) = unix.into_split();
    // Client → crabd runs from the start (the `auth` frame, possibly pipelined requests).
    let mut upstream = tokio::spawn(async move {
        let r = tokio::io::copy(&mut tcp_r, &mut unix_w).await;
        let _ = unix_w.shutdown().await;
        r
    });

    // crabd → client: the first frame decides (see the module docs).
    let first = ticket
        .run(shared.pending_timeout, codec::read_frame(&mut unix_r))
        .await;
    drop(ticket);
    let payload = match first {
        Ok(Ok(Some(p))) => p,
        Ok(Ok(None) | Err(_)) => {
            upstream.abort();
            debug!(%peer, "crabd closed the connection before answering");
            return;
        }
        Err(end) => {
            upstream.abort();
            shared.refusals.warn(
                &peer.to_string(),
                &format!("connection not authenticated in time ({end:?}); closed"),
            );
            return;
        }
    };
    let ok = accepted(&payload);
    let mut frame = Vec::with_capacity(HEADER_LEN + payload.len());
    // `read_frame` bounded the length by MAX_FRAME_LEN < u32::MAX.
    frame.extend_from_slice(
        &u32::try_from(payload.len())
            .unwrap_or(u32::MAX)
            .to_be_bytes(),
    );
    frame.extend_from_slice(&payload);
    let wrote = tcp_w.write_all(&frame).await;
    if !ok || wrote.is_err() {
        // crabd refused it and closes its side; do not keep the client's side open either.
        upstream.abort();
        if !ok {
            shared
                .refusals
                .warn(&peer.to_string(), "crabd refused the connection; closed");
        }
        let _ = tcp_w.shutdown().await;
        return;
    }
    let Ok(_slot) = Arc::clone(&shared.slots).try_acquire_owned() else {
        upstream.abort();
        shared
            .refusals
            .warn(&peer.to_string(), "relay connections full; closed");
        return;
    };
    let down = async {
        let r = tokio::io::copy(&mut unix_r, &mut tcp_w).await;
        let _ = tcp_w.shutdown().await;
        r
    };
    let (down, up) = tokio::join!(down, &mut upstream);
    match (down, up) {
        (Ok(d), Ok(Ok(u))) => debug!(%peer, up = u, down = d, "relayed connection closed"),
        (d, u) => debug!(
            %peer,
            down = ?d.err(),
            up = ?u.map(Result::err),
            "relayed connection ended"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sandcrab_protocol::{
        AUTH_TOKEN_BYTES, AuthToken, ProtocolError, RequestId, RpcResponse, encode_frame,
    };
    use tokio::io::AsyncReadExt;
    use tokio::net::UnixListener;

    fn ack() -> Vec<u8> {
        encode_frame(&RpcResponse::success(
            RequestId::Number(0),
            serde_json::json!({}),
        ))
        .unwrap()
    }

    fn refusal() -> Vec<u8> {
        encode_frame(&RpcResponse::failure(
            Some(RequestId::Number(0)),
            &ProtocolError::Unauthorized("authentication required".into()),
        ))
        .unwrap()
    }

    async fn start(cfg: RelayConfig) -> SocketAddr {
        let relay = Relay::bind(cfg).await.unwrap();
        let addr = relay.local_addr().unwrap();
        tokio::spawn(relay.run(std::future::pending()));
        addr
    }

    async fn read_eof_within(c: &mut TcpStream, secs: u64) -> Vec<u8> {
        let mut out = Vec::new();
        tokio::time::timeout(Duration::from_secs(secs), c.read_to_end(&mut out))
            .await
            .expect("relay closed the connection")
            .unwrap();
        out
    }

    #[tokio::test]
    async fn authenticated_connection_is_relayed_with_half_close() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("s.sock");
        let ul = UnixListener::bind(&sock).unwrap();
        // Fake crabd: accept the first frame, then echo until EOF and append "!".
        tokio::spawn(async move {
            while let Ok((mut s, _)) = ul.accept().await {
                tokio::spawn(async move {
                    codec::read_frame(&mut s).await.unwrap().unwrap();
                    s.write_all(&ack()).await.unwrap();
                    let mut buf = Vec::new();
                    s.read_to_end(&mut buf).await.unwrap();
                    buf.push(b'!');
                    s.write_all(&buf).await.unwrap();
                });
            }
        });
        let addr = start(RelayConfig {
            max_connections: 4,
            ..RelayConfig::new("127.0.0.1:0", sock)
        })
        .await;
        for _ in 0..6 {
            let mut c = TcpStream::connect(addr).await.unwrap();
            c.write_all(&encode_frame(&serde_json::json!({})).unwrap())
                .await
                .unwrap();
            let first = codec::read_frame(&mut c).await.unwrap().unwrap();
            assert!(accepted(&first));
            c.write_all(b"ping").await.unwrap();
            c.shutdown().await.unwrap();
            assert_eq!(read_eof_within(&mut c, 5).await, b"ping!");
        }
    }

    /// crabd refused the connection: the relay forwards the refusal and closes the client's
    /// side at once, even though the client (and here crabd too) would keep it open.
    #[tokio::test]
    async fn refused_connection_is_closed_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("s.sock");
        let ul = UnixListener::bind(&sock).unwrap();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = ul.accept().await {
                tokio::spawn(async move {
                    s.write_all(&refusal()).await.unwrap();
                    tokio::time::sleep(Duration::from_secs(60)).await;
                });
            }
        });
        let addr = start(RelayConfig {
            max_connections: 1,
            ..RelayConfig::new("127.0.0.1:0", sock)
        })
        .await;
        for _ in 0..3 {
            let mut c = TcpStream::connect(addr).await.unwrap();
            let out = read_eof_within(&mut c, 2).await;
            assert_eq!(out, refusal(), "refusal forwarded, then EOF");
        }
    }

    #[tokio::test]
    async fn missing_target_closes_the_connection() {
        let dir = tempfile::tempdir().unwrap();
        let addr = start(RelayConfig {
            max_connections: 1,
            ..RelayConfig::new("127.0.0.1:0", dir.path().join("nope.sock"))
        })
        .await;
        let mut c = TcpStream::connect(addr).await.unwrap();
        assert!(read_eof_within(&mut c, 2).await.is_empty());
    }

    /// A flood of idle, never-authenticating connections neither fills the relay's slots
    /// nor keeps the real client out. The real `crabd` (with its own pre-auth queue) sits
    /// behind the relay, with a deliberately long auth deadline on both.
    #[tokio::test]
    async fn unauthenticated_flood_cannot_starve_the_client() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("crabd.sock");
        let token = AuthToken::from_bytes(&[0x42; AUTH_TOKEN_BYTES]);
        let mut cfg = crate::ServerConfig::new(crate::ListenAddr::Unix(sock.clone()));
        cfg.auth = Some(crate::AuthConfig {
            token: token.clone(),
            timeout: Duration::from_secs(30),
        });
        cfg.max_connections = 2;
        let server = crate::Server::bind(cfg, Arc::new(crate::ToolContext::new(dir.path())))
            .await
            .unwrap();
        tokio::spawn(server.run(std::future::pending()));
        let addr = start(RelayConfig {
            max_connections: 2,
            max_pending: 4,
            pending_timeout: Duration::from_secs(30),
            ..RelayConfig::new("127.0.0.1:0", sock)
        })
        .await;

        let mut idle = Vec::new();
        for _ in 0..64 {
            idle.push(TcpStream::connect(addr).await.unwrap());
        }
        // Two real clients, one after the other, while the flood stays connected.
        let mut clients = Vec::new();
        for i in 0..2 {
            let mut c = TcpStream::connect(addr).await.unwrap();
            c.write_all(&encode_frame(&token.to_request(0)).unwrap())
                .await
                .unwrap();
            let ack = tokio::time::timeout(Duration::from_secs(5), codec::read_frame(&mut c))
                .await
                .unwrap_or_else(|_| panic!("client {i} not served during the flood"))
                .unwrap()
                .unwrap();
            assert!(accepted(&ack), "client {i}");
            let ping = serde_json::json!({ "jsonrpc": "2.0", "id": 1, "method": "ping" });
            c.write_all(&encode_frame(&ping).unwrap()).await.unwrap();
            let pong = codec::read_frame(&mut c).await.unwrap().unwrap();
            assert!(accepted(&pong), "client {i} ping");
            clients.push(c);
            for _ in 0..16 {
                idle.push(TcpStream::connect(addr).await.unwrap());
            }
        }
        drop(idle);
    }
}
