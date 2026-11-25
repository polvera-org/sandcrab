//! `tunnel.open`: relay a dedicated host connection to a TCP port on the guest's loopback.
//!
//! The server hands a connection here when its **first** frame is a `tunnel.open` request
//! (see [`sandcrab_protocol::tunnel`]). We then:
//!
//! 1. validate the params (port `1..=65535`, nothing else accepted);
//! 2. take a tunnel slot ([`TunnelConfig::max_tunnels`]); when none is free the request fails
//!    at once with `EXECUTION_FAILED` instead of queueing;
//! 3. connect to `127.0.0.1:<port>`, falling back to `[::1]:<port>`, within
//!    [`TunnelConfig::connect_timeout`]. The target is always loopback;
//! 4. write the ack frame, then copy bytes both ways with fixed-size buffers until both
//!    directions reach EOF (half-close is propagated), either side errors, or the server
//!    shuts down.
//!
//! **vsock transports** ([`TunnelConfig::close_on_target_eof`], set by the server for vsock
//! listeners): when the target reaches EOF, its bytes are flushed and the whole tunnel is
//! closed instead of half-closed. Firecracker's vsock bridge never forwards a guest
//! `SHUT_WR` to the host, so without this a client that reads until the server closes
//! (`Connection: close` without `Content-Length`, HTTP/1.0) would never see the end. The
//! cost: after the target half-closes, later host → guest bytes are dropped (a target that
//! half-closes first and keeps reading is rare). Unix-socket transports (Docker) keep full
//! half-close semantics.
//!
//! The relay is strictly 1:1: every byte read from one side is written once to the other,
//! with at most [`RELAY_BUF`] bytes buffered per direction. Nothing is generated or
//! repeated, so a tunnel cannot amplify traffic.

use std::{
    io,
    net::{Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::Arc,
    time::Duration,
};

use sandcrab_protocol::{ProtocolError, RpcRequest, RpcResponse, TunnelOpenAck, TunnelOpenParams};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::TcpStream,
    sync::{Semaphore, watch},
};
use tracing::{debug, info, warn};

use crate::codec;

/// Default max concurrently open tunnels per `crabd serve`.
pub const DEFAULT_MAX_TUNNELS: usize = 64;
/// Default budget for connecting to the guest target.
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Relay buffer per direction.
pub const RELAY_BUF: usize = 32 * 1024;

/// Tunnel limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TunnelConfig {
    /// Max tunnels open at once. Tunnels do not count against `max_connections`.
    pub max_tunnels: usize,
    /// Time allowed to connect to the guest loopback target.
    pub connect_timeout: Duration,
    /// Close the whole tunnel (after flushing) when the target reaches EOF, instead of
    /// half-closing the host side. For transports that drop half-close (vsock).
    pub close_on_target_eof: bool,
}

impl Default for TunnelConfig {
    fn default() -> Self {
        Self {
            max_tunnels: DEFAULT_MAX_TUNNELS,
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
            close_on_target_eof: false,
        }
    }
}

/// Shared tunnel state of one server: the slot semaphore and the limits.
#[derive(Debug)]
pub struct TunnelService {
    slots: Arc<Semaphore>,
    config: TunnelConfig,
}

impl TunnelService {
    #[must_use]
    pub fn new(config: TunnelConfig) -> Self {
        Self {
            slots: Arc::new(Semaphore::new(config.max_tunnels)),
            config,
        }
    }

    /// Tunnels currently open.
    #[must_use]
    pub fn open_tunnels(&self) -> usize {
        self.config.max_tunnels - self.slots.available_permits()
    }

    /// Serve a `tunnel.open` request that arrived as the first frame of `stream`. Returns
    /// when the tunnel is closed (or was refused).
    pub async fn serve<S>(
        &self,
        mut stream: S,
        req: RpcRequest,
        mut stop: watch::Receiver<bool>,
        peer: &str,
    ) where
        S: AsyncRead + AsyncWrite + Unpin + Send,
    {
        let id = req.id.clone();
        let refuse = |e: ProtocolError| RpcResponse::failure(Some(id.clone()), &e);

        let params = match TunnelOpenParams::from_request(&req) {
            Ok(p) => p,
            Err(e) => {
                debug!(%peer, error = %e, "bad tunnel.open");
                let _ = codec::write_frame(&mut stream, &refuse(e)).await;
                return;
            }
        };
        let port = params.port;
        let Ok(_slot) = Arc::clone(&self.slots).try_acquire_owned() else {
            warn!(%peer, port, limit = self.config.max_tunnels, "tunnel refused: limit reached");
            let e = ProtocolError::ExecutionFailed(format!(
                "too many open tunnels (limit {}); close one and retry",
                self.config.max_tunnels
            ));
            let _ = codec::write_frame(&mut stream, &refuse(e)).await;
            return;
        };

        let target = tokio::select! {
            biased;
            _ = stop.wait_for(|s| *s) => return,
            t = tokio::time::timeout(self.config.connect_timeout, connect_loopback(port)) => t,
        };
        let mut target = match target {
            Ok(Ok(t)) => t,
            Ok(Err(e)) => {
                debug!(%peer, port, error = %e, "tunnel target unreachable");
                let e = if e.kind() == io::ErrorKind::ConnectionRefused {
                    ProtocolError::NotFound(format!(
                        "nothing is listening on guest port {port} (127.0.0.1 or [::1])"
                    ))
                } else {
                    ProtocolError::ExecutionFailed(format!(
                        "connect to guest port {port} failed: {e}"
                    ))
                };
                let _ = codec::write_frame(&mut stream, &refuse(e)).await;
                return;
            }
            Err(_) => {
                let ms = u64::try_from(self.config.connect_timeout.as_millis()).unwrap_or(u64::MAX);
                let _ = codec::write_frame(
                    &mut stream,
                    &refuse(ProtocolError::TimedOut { timeout_ms: ms }),
                )
                .await;
                return;
            }
        };
        let _ = target.set_nodelay(true);

        let ack = RpcResponse::success(id, TunnelOpenAck { port }.to_value());
        if let Err(e) = codec::write_frame(&mut stream, &ack).await {
            debug!(%peer, port, error = %e, "tunnel ack failed");
            return;
        }
        info!(%peer, port, "tunnel open");

        let close_on_target_eof = self.config.close_on_target_eof;
        let relay = async {
            if close_on_target_eof {
                relay_closing_on_target_eof(&mut stream, &mut target).await
            } else {
                tokio::io::copy_bidirectional_with_sizes(
                    &mut stream,
                    &mut target,
                    RELAY_BUF,
                    RELAY_BUF,
                )
                .await
            }
        };
        tokio::select! {
            biased;
            _ = stop.wait_for(|s| *s) => info!(%peer, port, "tunnel closed: server shutting down"),
            r = relay => match r {
                Ok((up, down)) => info!(%peer, port, host_to_guest = up, guest_to_host = down, "tunnel closed"),
                Err(e) => debug!(%peer, port, error = %e, "tunnel ended with error"),
            },
        }
    }
}

/// Relay for transports without half-close: host → guest EOF is still propagated as a
/// `SHUT_WR` to the target, but the target's EOF ends the tunnel (after its bytes are flushed
/// to the host), which the host sees as a full close.
async fn relay_closing_on_target_eof<S>(
    stream: &mut S,
    target: &mut TcpStream,
) -> io::Result<(u64, u64)>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let (mut host_r, mut host_w) = tokio::io::split(stream);
    let (mut target_r, mut target_w) = target.split();
    let up = async {
        let mut buf = vec![0u8; RELAY_BUF];
        let mut total = 0u64;
        loop {
            let n = host_r.read(&mut buf).await?;
            if n == 0 {
                target_w.shutdown().await?;
                return Ok::<_, io::Error>(total);
            }
            target_w.write_all(&buf[..n]).await?;
            total += n as u64;
        }
    };
    let down = async {
        let mut buf = vec![0u8; RELAY_BUF];
        let mut total = 0u64;
        loop {
            let n = target_r.read(&mut buf).await?;
            if n == 0 {
                host_w.flush().await?;
                return Ok::<_, io::Error>(total);
            }
            host_w.write_all(&buf[..n]).await?;
            total += n as u64;
        }
    };
    tokio::pin!(up, down);
    let mut up_total = None;
    loop {
        tokio::select! {
            r = &mut up, if up_total.is_none() => up_total = Some(r?),
            // Returning drops both halves: the caller closes the host stream.
            r = &mut down => return Ok((up_total.unwrap_or(0), r?)),
        }
    }
}

/// Connect to `127.0.0.1:<port>`, then `[::1]:<port>` (servers bound to `localhost` may
/// listen on either). Both are guest loopback; nothing else is ever dialled.
async fn connect_loopback(port: u16) -> io::Result<TcpStream> {
    let v4 = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    match TcpStream::connect(v4).await {
        Ok(s) => Ok(s),
        Err(e4) => {
            let v6 = SocketAddr::from((Ipv6Addr::LOCALHOST, port));
            TcpStream::connect(v6).await.map_err(|_| e4)
        }
    }
}

/// True when `payload` is a `tunnel.open` request (any params).
#[must_use]
pub fn as_tunnel_open(payload: &[u8]) -> Option<RpcRequest> {
    match sandcrab_protocol::RpcMessage::from_slice(payload) {
        Ok(sandcrab_protocol::RpcMessage::Request(r))
            if r.method == sandcrab_protocol::METHOD_TUNNEL_OPEN =>
        {
            Some(r)
        }
        _ => None,
    }
}
