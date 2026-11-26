//! Forwards, guest side: loopback ports inside the sandbox whose connections the **host**
//! routes to a target it chose (see [`sandcrab_protocol::forward`]).
//!
//! - **Fixed ports, bound at start.** [`ForwardService::bind`] binds `127.0.0.1:<p>` for each
//!   configured port, and `[::1]:<p>` when the guest has IPv6 loopback. Never a wildcard
//!   address. A port that cannot be bound fails the start. `Server::bind` calls it, so the
//!   listeners exist before `hello` can be answered.
//! - **`forward.listen`** (first frame of a host connection): that connection becomes the
//!   announce channel. One at a time: a second gets `-32001` and is closed while the first
//!   is alive. For every guest connection `crabd` sends `forward.incoming {conn_id, port}` on
//!   it; the host may answer with a `forward.reject {conn_id}` notification.
//! - **`forward.accept {conn_id}`** (first frame of a new host connection): takes the
//!   announced connection, answers `{port}`, then relays `DATA` / `EOF` records ↔ raw bytes
//!   with fixed buffers ([`RELAY_BUF`] per direction). An unknown or used id is `-32004`.
//! - **`forward.stats`**: per-port counters since start ([`ForwardService::stats`]).
//!
//! **Fail closed.** A guest connection is closed at once, and one counter goes up, when no
//! listen connection is open (`no_listener`), when a cap is reached (`cap`: connections per
//! forward, per sandbox, or announced and not yet accepted), when it is not accepted within
//! [`ForwardConfig::unclaimed_timeout`] (`unclaimed`), or on `forward.reject` (`rejected`).
//! Nothing queues beyond [`ForwardConfig::max_pending`].
//!
//! **Records, not raw bytes.** `EOF` is an explicit half-close in both directions on every
//! transport (vsock bridges drop `SHUT_WR`). A guest `EOF` becomes an `EOF` record; an `EOF`
//! record becomes `shutdown(SHUT_WR)` towards the program. The connection ends when both
//! directions have ended (then `crabd` closes the host connection), when the program's side
//! fails, or when the host connection drops without an `EOF` record (then the program's
//! connection is closed).

use std::{
    collections::HashMap,
    io,
    net::{Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use sandcrab_protocol::{
    ForwardAcceptAck, ForwardAcceptParams, ForwardIncoming, ForwardListenAck, ForwardListenParams,
    ForwardPortStats, ForwardRefused, ForwardRejectParams, ForwardStatsResult,
    METHOD_FORWARD_ACCEPT, METHOD_FORWARD_LISTEN, METHOD_FORWARD_REJECT, ProtocolError,
    RECORD_HEADER_LEN, RecordChannel, RecordKind, RpcMessage, RpcRequest, RpcResponse,
    encode_record_header, forward as fwd, parse_record_header,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{OwnedSemaphorePermit, Semaphore, mpsc, watch},
    task::JoinSet,
};
use tracing::{debug, info, warn};

use crate::codec;

/// Relay buffer per direction in the guest.
pub const RELAY_BUF: usize = 32 * 1024;

/// Forward ports and limits of one `crabd serve`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForwardConfig {
    /// Guest loopback ports (`--forward-port`). At most [`fwd::MAX_FORWARDS`], no duplicates.
    pub ports: Vec<u16>,
    /// Connections at once per forward (announced or relayed).
    pub max_conns_per_forward: usize,
    /// Connections at once, all forwards (`--max-forward-conns`).
    pub max_conns: usize,
    /// Announced and not yet accepted, all forwards.
    pub max_pending: usize,
    /// An announced connection not accepted within this long is closed.
    pub unclaimed_timeout: Duration,
}

impl Default for ForwardConfig {
    fn default() -> Self {
        Self {
            ports: Vec::new(),
            max_conns_per_forward: fwd::DEFAULT_CONNS_PER_FORWARD as usize,
            max_conns: fwd::DEFAULT_MAX_FORWARD_CONNS as usize,
            max_pending: fwd::MAX_PENDING_FORWARD_CONNS as usize,
            unclaimed_timeout: Duration::from_millis(fwd::FORWARD_UNCLAIMED_TIMEOUT_MS),
        }
    }
}

impl ForwardConfig {
    /// Ports `1..=65535`, no duplicates, at most [`fwd::MAX_FORWARDS`]; every cap at least 1.
    pub fn validate(&self) -> io::Result<()> {
        let invalid = |m: String| Err(io::Error::new(io::ErrorKind::InvalidInput, m));
        if self.ports.len() > fwd::MAX_FORWARDS {
            return invalid(format!("at most {} forward ports", fwd::MAX_FORWARDS));
        }
        for (i, &p) in self.ports.iter().enumerate() {
            if p == 0 {
                return invalid("forward port 0 is not valid".into());
            }
            if self.ports[..i].contains(&p) {
                return invalid(format!("forward port {p} is given twice"));
            }
        }
        if self.max_conns_per_forward == 0 || self.max_conns == 0 || self.max_pending == 0 {
            return invalid("forward connection caps must be at least 1".into());
        }
        Ok(())
    }
}

/// Bound forward listeners, handed to [`ForwardService::serve_port`] by the server.
#[derive(Debug)]
pub struct ForwardListeners(Vec<(usize, TcpListener)>);

impl ForwardListeners {
    /// Local addresses (tests, logs).
    #[must_use]
    pub fn local_addrs(&self) -> Vec<SocketAddr> {
        self.0
            .iter()
            .filter_map(|(_, l)| l.local_addr().ok())
            .collect()
    }
}

#[derive(Debug, Default)]
struct Counters {
    accepted: AtomicU64,
    no_listener: AtomicU64,
    cap: AtomicU64,
    unclaimed: AtomicU64,
    rejected: AtomicU64,
}

#[derive(Debug)]
struct PortState {
    port: u16,
    slots: Arc<Semaphore>,
    counters: Counters,
}

/// An announced guest connection waiting for `forward.accept`.
#[derive(Debug)]
struct Pending {
    idx: usize,
    stream: TcpStream,
    _permits: (OwnedSemaphorePermit, OwnedSemaphorePermit),
}

/// The open `forward.listen` connection.
#[derive(Debug)]
struct Announcer {
    generation: u64,
    tx: mpsc::Sender<ForwardIncoming>,
}

/// Forward state of one server: ports, caps, counters, the announce channel and the
/// announced connections.
#[derive(Debug)]
pub struct ForwardService {
    config: ForwardConfig,
    ports: Vec<PortState>,
    total: Arc<Semaphore>,
    next_id: AtomicU64,
    pending: Mutex<HashMap<u64, Pending>>,
    announcer: Mutex<Option<Announcer>>,
}

/// Why a guest connection was refused.
enum Refusal {
    NoListener,
    Cap,
}

impl ForwardService {
    /// Validate `config` and bind every forward port on loopback.
    pub async fn bind(config: ForwardConfig) -> io::Result<(Arc<Self>, ForwardListeners)> {
        config.validate()?;
        let mut listeners = Vec::new();
        for (idx, &port) in config.ports.iter().enumerate() {
            let v4 = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
            let l = TcpListener::bind(v4).await.map_err(|e| {
                io::Error::new(e.kind(), format!("forward port {port}: bind {v4}: {e}"))
            })?;
            listeners.push((idx, l));
            let v6 = SocketAddr::from((Ipv6Addr::LOCALHOST, port));
            match TcpListener::bind(v6).await {
                Ok(l) => listeners.push((idx, l)),
                Err(e) if no_ipv6_loopback(&e) => {
                    debug!(port, error = %e, "no IPv6 loopback; forward bound on 127.0.0.1 only");
                }
                Err(e) => {
                    return Err(io::Error::new(
                        e.kind(),
                        format!("forward port {port}: bind {v6}: {e}"),
                    ));
                }
            }
        }
        let ports = config
            .ports
            .iter()
            .map(|&port| PortState {
                port,
                slots: Arc::new(Semaphore::new(config.max_conns_per_forward)),
                counters: Counters::default(),
            })
            .collect();
        let svc = Arc::new(Self {
            total: Arc::new(Semaphore::new(config.max_conns)),
            ports,
            next_id: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
            announcer: Mutex::new(None),
            config,
        });
        if !svc.config.ports.is_empty() {
            info!(ports = ?svc.config.ports, "forward ports bound on loopback");
        }
        Ok((svc, ForwardListeners(listeners)))
    }

    /// The forward ports, in configured order.
    #[must_use]
    pub fn ports(&self) -> Vec<u16> {
        self.config.ports.clone()
    }

    /// `forward.stats`: counters per port since start.
    #[must_use]
    pub fn stats(&self) -> ForwardStatsResult {
        ForwardStatsResult(
            self.ports
                .iter()
                .map(|p| {
                    let c = &p.counters;
                    ForwardPortStats {
                        port: p.port,
                        accepted: c.accepted.load(Ordering::Relaxed),
                        refused: ForwardRefused {
                            no_listener: c.no_listener.load(Ordering::Relaxed),
                            cap: c.cap.load(Ordering::Relaxed),
                            unclaimed: c.unclaimed.load(Ordering::Relaxed),
                            rejected: c.rejected.load(Ordering::Relaxed),
                        },
                    }
                })
                .collect(),
        )
    }

    /// Spawn one accept loop per bound listener into `tasks`.
    pub fn serve_ports(self: &Arc<Self>, listeners: ForwardListeners, tasks: &mut JoinSet<()>) {
        for (idx, l) in listeners.0 {
            tasks.spawn(Arc::clone(self).serve_port(idx, l));
        }
    }

    /// Drop the announce channel and every announced connection (server shutdown).
    pub fn shutdown(&self) {
        self.announcer.lock().expect("announcer lock").take();
        self.pending.lock().expect("pending lock").clear();
    }

    async fn serve_port(self: Arc<Self>, idx: usize, listener: TcpListener) {
        // Owns the deadline timers: aborting this task drops them.
        let mut deadlines = JoinSet::new();
        loop {
            while deadlines.try_join_next().is_some() {}
            let stream = match listener.accept().await {
                Ok((s, _)) => s,
                Err(e) => {
                    warn!(port = self.ports[idx].port, error = %e, "forward accept failed");
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                }
            };
            if let Some(id) = self.admit(idx, stream) {
                let svc = Arc::clone(&self);
                deadlines.spawn(async move {
                    tokio::time::sleep(svc.config.unclaimed_timeout).await;
                    if svc.take_pending(id).is_some() {
                        svc.ports[idx]
                            .counters
                            .unclaimed
                            .fetch_add(1, Ordering::Relaxed);
                        debug!(
                            conn_id = id,
                            "forwarded connection not accepted in time; closed"
                        );
                    }
                });
            }
        }
    }

    /// Announce a guest connection, or refuse it (the stream is dropped: closed at once).
    fn admit(&self, idx: usize, stream: TcpStream) -> Option<u64> {
        let port = &self.ports[idx];
        match self.try_announce(idx, stream) {
            Ok(id) => Some(id),
            Err(why) => {
                let (counter, what) = match why {
                    Refusal::NoListener => (&port.counters.no_listener, "no forward.listen"),
                    Refusal::Cap => (&port.counters.cap, "cap reached"),
                };
                counter.fetch_add(1, Ordering::Relaxed);
                debug!(port = port.port, why = what, "forwarded connection refused");
                None
            }
        }
    }

    fn try_announce(&self, idx: usize, stream: TcpStream) -> Result<u64, Refusal> {
        let port = &self.ports[idx];
        let tx = match &*self.announcer.lock().expect("announcer lock") {
            Some(a) if !a.tx.is_closed() => a.tx.clone(),
            _ => return Err(Refusal::NoListener),
        };
        let id = {
            let mut pending = self.pending.lock().expect("pending lock");
            if pending.len() >= self.config.max_pending {
                return Err(Refusal::Cap);
            }
            let per_forward = Arc::clone(&port.slots)
                .try_acquire_owned()
                .map_err(|_| Refusal::Cap)?;
            let total = Arc::clone(&self.total)
                .try_acquire_owned()
                .map_err(|_| Refusal::Cap)?;
            let id = self.next_id.fetch_add(1, Ordering::Relaxed);
            pending.insert(
                id,
                Pending {
                    idx,
                    stream,
                    _permits: (per_forward, total),
                },
            );
            id
        };
        let announce = ForwardIncoming {
            conn_id: id,
            port: port.port,
        };
        match tx.try_send(announce) {
            Ok(()) => Ok(id),
            Err(e) => {
                self.take_pending(id);
                Err(match e {
                    mpsc::error::TrySendError::Full(_) => Refusal::Cap,
                    mpsc::error::TrySendError::Closed(_) => Refusal::NoListener,
                })
            }
        }
    }

    fn take_pending(&self, id: u64) -> Option<Pending> {
        self.pending.lock().expect("pending lock").remove(&id)
    }

    fn reject(&self, id: u64) {
        if let Some(p) = self.take_pending(id) {
            self.ports[p.idx]
                .counters
                .rejected
                .fetch_add(1, Ordering::Relaxed);
            debug!(
                conn_id = id,
                "forwarded connection rejected by the host; closed"
            );
        }
    }

    /// Serve a connection whose first frame is `forward.listen`. Returns when it closes.
    pub async fn serve_listen<S>(
        &self,
        mut stream: S,
        req: RpcRequest,
        mut stop: watch::Receiver<bool>,
        peer: &str,
    ) where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        if let Err(e) = ForwardListenParams::from_request(&req) {
            let _ = codec::write_frame(&mut stream, &RpcResponse::failure(Some(req.id), &e)).await;
            return;
        }
        let (tx, mut rx) = mpsc::channel(self.config.max_pending);
        let generation = self.next_id.fetch_add(1, Ordering::Relaxed);
        let registered = {
            let mut slot = self.announcer.lock().expect("announcer lock");
            let busy = slot.as_ref().is_some_and(|a| !a.tx.is_closed());
            if !busy {
                *slot = Some(Announcer { generation, tx });
            }
            !busy
        };
        if !registered {
            warn!(%peer, "second forward.listen refused");
            let e = ProtocolError::ExecutionFailed(
                "a forward.listen connection is already open".into(),
            );
            let _ = codec::write_frame(&mut stream, &RpcResponse::failure(Some(req.id), &e)).await;
            return;
        }
        let ack = RpcResponse::success(
            req.id,
            ForwardListenAck {
                ports: self.ports(),
            }
            .to_value(),
        );
        if codec::write_frame(&mut stream, &ack).await.is_ok() {
            info!(%peer, "forward.listen connection open");
            let (mut rd, mut wr) = tokio::io::split(stream);
            // Reads `forward.reject` notifications until the host closes. A separate future
            // because `read_frame` is not cancel-safe.
            let reader = async {
                loop {
                    let payload = match codec::read_frame(&mut rd).await {
                        Ok(Some(p)) => p,
                        Ok(None) => return,
                        Err(e) => {
                            debug!(%peer, error = %e, "forward.listen read ended");
                            return;
                        }
                    };
                    match RpcMessage::from_slice(&payload) {
                        Ok(RpcMessage::Notification(n)) if n.method == METHOD_FORWARD_REJECT => {
                            match ForwardRejectParams::from_notification(&n) {
                                Ok(p) => self.reject(p.conn_id),
                                Err(e) => debug!(%peer, error = %e, "bad forward.reject"),
                            }
                        }
                        _ => debug!(%peer, "ignoring frame on the forward.listen connection"),
                    }
                }
            };
            let writer = async {
                while let Some(inc) = rx.recv().await {
                    if codec::write_frame(&mut wr, &inc.to_notification())
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
            };
            tokio::select! {
                biased;
                _ = stop.wait_for(|s| *s) => {}
                () = reader => {}
                () = writer => {}
            }
            info!(%peer, "forward.listen connection closed");
        }
        let mut slot = self.announcer.lock().expect("announcer lock");
        if slot.as_ref().is_some_and(|a| a.generation == generation) {
            *slot = None;
        }
    }

    /// Serve a connection whose first frame is `forward.accept`. Returns when the forwarded
    /// connection ends.
    pub async fn serve_accept<S>(
        &self,
        mut stream: S,
        req: RpcRequest,
        mut stop: watch::Receiver<bool>,
        peer: &str,
    ) where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let id = req.id.clone();
        let refuse = |e: ProtocolError| RpcResponse::failure(Some(id.clone()), &e);
        let conn_id = match ForwardAcceptParams::from_request(&req) {
            Ok(p) => p.conn_id,
            Err(e) => {
                let _ = codec::write_frame(&mut stream, &refuse(e)).await;
                return;
            }
        };
        let Some(pending) = self.take_pending(conn_id) else {
            let e = ProtocolError::NotFound(format!("forward conn_id {conn_id} (unknown or used)"));
            let _ = codec::write_frame(&mut stream, &refuse(e)).await;
            return;
        };
        let state = &self.ports[pending.idx];
        state.counters.accepted.fetch_add(1, Ordering::Relaxed);
        let port = state.port;
        let ack = RpcResponse::success(id, ForwardAcceptAck { port }.to_value());
        if let Err(e) = codec::write_frame(&mut stream, &ack).await {
            debug!(%peer, conn_id, error = %e, "forward.accept ack failed");
            return;
        }
        let Pending {
            stream: mut target,
            _permits,
            ..
        } = pending;
        let _ = target.set_nodelay(true);
        debug!(%peer, conn_id, port, "forwarded connection accepted");
        tokio::select! {
            biased;
            _ = stop.wait_for(|s| *s) => debug!(%peer, conn_id, "forward closed: server shutting down"),
            r = relay(&mut stream, &mut target) => match r {
                Ok((up, down)) => debug!(%peer, conn_id, host_to_guest = up, guest_to_host = down, "forward closed"),
                Err(e) => debug!(%peer, conn_id, error = %e, "forward ended"),
            },
        }
    }
}

/// `EADDRNOTAVAIL` / `EAFNOSUPPORT`: the guest has no IPv6 loopback.
fn no_ipv6_loopback(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::AddrNotAvailable
        || e.raw_os_error() == Some(rustix::io::Errno::AFNOSUPPORT.raw_os_error())
}

/// Relay records on `host` ↔ bytes on `target` until both directions ended. An error ends
/// both: the caller drops both streams.
async fn relay<S>(host: &mut S, target: &mut TcpStream) -> io::Result<(u64, u64)>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let (mut host_r, mut host_w) = tokio::io::split(host);
    let (mut target_r, mut target_w) = target.split();
    // Host → guest: DATA records become bytes, EOF becomes SHUT_WR.
    let up = async {
        let mut buf = vec![0u8; RELAY_BUF];
        let mut total = 0u64;
        loop {
            let mut header = [0u8; RECORD_HEADER_LEN];
            host_r.read_exact(&mut header).await?;
            let (kind, mut len) = parse_record_header(RecordChannel::Forward, header)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            if kind == RecordKind::Eof {
                target_w.shutdown().await?;
                return Ok::<_, io::Error>(total);
            }
            while len > 0 {
                let n = len.min(RELAY_BUF);
                host_r.read_exact(&mut buf[..n]).await?;
                target_w.write_all(&buf[..n]).await?;
                len -= n;
                total += n as u64;
            }
        }
    };
    // Guest → host: bytes become DATA records, EOF becomes an EOF record.
    let down = async {
        let mut buf = vec![0u8; RECORD_HEADER_LEN + RELAY_BUF];
        let mut total = 0u64;
        loop {
            let n = target_r.read(&mut buf[RECORD_HEADER_LEN..]).await?;
            let kind = if n == 0 {
                RecordKind::Eof
            } else {
                RecordKind::Data
            };
            let header = encode_record_header(kind, n)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            buf[..RECORD_HEADER_LEN].copy_from_slice(&header);
            host_w.write_all(&buf[..RECORD_HEADER_LEN + n]).await?;
            host_w.flush().await?;
            if n == 0 {
                return Ok::<_, io::Error>(total);
            }
            total += n as u64;
        }
    };
    let (up, down) = tokio::try_join!(up, down)?;
    let _ = host_w.shutdown().await;
    Ok((up, down))
}

/// The request if `payload` is a `forward.listen` or `forward.accept` request.
#[must_use]
pub fn as_forward_verb(payload: &[u8]) -> Option<RpcRequest> {
    match RpcMessage::from_slice(payload) {
        Ok(RpcMessage::Request(r))
            if r.method == METHOD_FORWARD_LISTEN || r.method == METHOD_FORWARD_ACCEPT =>
        {
            Some(r)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_validation() {
        let ok = ForwardConfig {
            ports: vec![1, 65535],
            ..ForwardConfig::default()
        };
        ok.validate().unwrap();
        for ports in [vec![0], vec![80, 80], (1..=17).collect::<Vec<u16>>()] {
            let c = ForwardConfig {
                ports: ports.clone(),
                ..ForwardConfig::default()
            };
            assert!(c.validate().is_err(), "{ports:?}");
        }
        let zero = ForwardConfig {
            max_pending: 0,
            ..ForwardConfig::default()
        };
        assert!(zero.validate().is_err());
        ForwardConfig {
            ports: (1..=16).collect(),
            ..ForwardConfig::default()
        }
        .validate()
        .unwrap();
    }
}
