//! Host side of forwards (design §4.3): a loopback port inside the sandbox whose connections
//! go to a target the caller chooses. The sandbox keeps network `disabled`; every connection
//! is opened by the host.
//!
//! [`ForwardAcceptor`] holds the sandbox's `forward.listen` connection. For each
//! `forward.incoming {conn_id, port}` it:
//!
//! 1. refuses (sends `forward.reject`) a port that is not one of the sandbox's forwards, or
//!    a connection over the forward's `max_connections` (default 64) or over the sandbox cap
//!    ([`ForwardConfig::max_connections`], default 256);
//! 2. asks the caller's [`ForwardTarget`] for a stream, within
//!    [`ForwardConfig::target_timeout`]; an error refuses the connection;
//! 3. opens a fresh `forward.accept {conn_id}` connection through the sandbox's
//!    [`Connector`] (so `auth` comes first) and relays between it and the target stream.
//!
//! **Relay.** `DATA` records carry the bytes, at most 64 KiB each, with one 64 KiB buffer per
//! direction. End-of-file of the target becomes an `EOF` record; an `EOF` record from the
//! guest becomes a `shutdown` (half-close) of the target. The transport connection itself is
//! never half-closed: the vsock bridges turn a host `SHUT_WR` into a full close or a broken
//! stream (`docs/protocol.md`, "Half-close over vsock"), so the end of one direction travels
//! only as a record, and the connection is closed once both directions ended. The record
//! reader holds at most one record.
//!
//! **Fails closed.** Any error on either side (target, transport, a bad record) drops both
//! streams, so the program in the sandbox sees its connection close.
//!
//! **Lifetime.** If the listen connection drops, the acceptor reconnects with backoff (until
//! then the guest refuses connections, counted as `no_listener`). It stops on
//! [`ForwardAcceptor::shutdown`], on drop, or when the `until` signal resolves, and every
//! relayed connection is cut with it, like [`crate::PortForwards`].

use std::collections::HashMap;
use std::fmt;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use sandcrab_protocol::{
    ForwardAcceptAck, ForwardAcceptParams, ForwardIncoming, ForwardListenAck, ForwardListenParams,
    ForwardRejectParams, METHOD_FORWARD_INCOMING, ProtocolError, RECORD_HEADER_LEN, RecordChannel,
    RecordKind, RpcMessage, encode_frame, encode_record_header,
    forward::{DEFAULT_CONNS_PER_FORWARD, DEFAULT_MAX_FORWARD_CONNS, FORWARD_UNCLAIMED_TIMEOUT_MS},
    parse_record_header,
    record::MAX_RECORD_DATA_LEN,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{Semaphore, mpsc, oneshot, watch};
use tokio::task::{JoinHandle, JoinSet};
use tracing::{debug, warn};

use crate::client::{BoxedStream, Connector, read_one_message, read_one_response};
use crate::tunnel::StopSignal;
use crate::{ForwardSpec, Result, SandboxError, SandboxId};

/// Decides where a forwarded connection goes. Called once per connection.
#[async_trait]
pub trait ForwardTarget: Send + Sync {
    /// A stream to the target for a program in `sandbox` that connected to `guest_port`.
    /// An error refuses the connection (the guest counts it as `rejected`).
    async fn connect(&self, sandbox: SandboxId, guest_port: u16) -> io::Result<BoxedStream>;
}

/// Limits and timings of a [`ForwardAcceptor`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ForwardConfig {
    /// Relayed connections at once, all forwards of the sandbox.
    pub max_connections: u32,
    /// Bound on [`ForwardTarget::connect`]. Defaults to the guest's unclaimed timeout: a
    /// later accept would find the connection gone.
    pub target_timeout: Duration,
    /// Bound on connect, `auth` and `forward.accept` for one connection, and on opening the
    /// listen connection.
    pub open_timeout: Duration,
    /// First and largest delay between attempts to (re)open the listen connection.
    pub reconnect_min: Duration,
    pub reconnect_max: Duration,
}

impl Default for ForwardConfig {
    fn default() -> Self {
        Self {
            max_connections: DEFAULT_MAX_FORWARD_CONNS,
            target_timeout: Duration::from_millis(FORWARD_UNCLAIMED_TIMEOUT_MS),
            open_timeout: Duration::from_secs(10),
            reconnect_min: Duration::from_millis(50),
            reconnect_max: Duration::from_secs(5),
        }
    }
}

/// Serves the forwards of one sandbox. Stops on [`Self::shutdown`], on drop, or when the
/// `until` signal resolves.
pub struct ForwardAcceptor {
    listening: watch::Receiver<bool>,
    stop: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
}

impl fmt::Debug for ForwardAcceptor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ForwardAcceptor")
            .field("listening", &self.is_listening())
            .finish_non_exhaustive()
    }
}

/// Shared by the listen loop and every connection task.
struct Ctx {
    sandbox: SandboxId,
    connector: Arc<dyn Connector>,
    target: Arc<dyn ForwardTarget>,
    config: ForwardConfig,
    /// Per guest port, its `max_connections` slots.
    ports: HashMap<u16, Arc<Semaphore>>,
    total: Arc<Semaphore>,
}

impl ForwardAcceptor {
    /// Validate `forwards` and start serving them. Must be called inside a tokio runtime.
    pub fn start(
        sandbox: SandboxId,
        connector: Arc<dyn Connector>,
        forwards: &[ForwardSpec],
        target: Arc<dyn ForwardTarget>,
        config: ForwardConfig,
        until: Option<StopSignal>,
    ) -> Result<Self> {
        if forwards.is_empty() {
            return Err(SandboxError::InvalidConfig(
                "the sandbox has no forwards to serve".into(),
            ));
        }
        if config.max_connections == 0 {
            return Err(SandboxError::InvalidConfig(
                "max_connections must be at least 1".into(),
            ));
        }
        let mut ports = HashMap::new();
        for f in forwards {
            let max = f.max_connections.unwrap_or(DEFAULT_CONNS_PER_FORWARD);
            if f.guest_port == 0 || max == 0 {
                return Err(SandboxError::InvalidConfig(format!(
                    "forward {}: guest_port and max_connections must be positive",
                    f.guest_port
                )));
            }
            if ports
                .insert(f.guest_port, Arc::new(Semaphore::new(max as usize)))
                .is_some()
            {
                return Err(SandboxError::InvalidConfig(format!(
                    "forward guest port {} is listed twice",
                    f.guest_port
                )));
            }
        }
        let ctx = Arc::new(Ctx {
            sandbox,
            connector,
            target,
            config,
            ports,
            total: Arc::new(Semaphore::new(config.max_connections as usize)),
        });
        let (listening_tx, listening) = watch::channel(false);
        let (stop, stop_rx) = oneshot::channel::<()>();
        let until = until.unwrap_or_else(|| Box::pin(std::future::pending()));
        let task = tokio::spawn(async move {
            let mut conns = JoinSet::new();
            tokio::select! {
                _ = stop_rx => {}
                () = until => debug!(%sandbox, "sandbox gone; stopping its forwards"),
                () = listen_loop(&ctx, &listening_tx, &mut conns) => {}
            }
            let _ = listening_tx.send(false);
            // Dropping the set aborts every relay: both of its streams close.
            conns.abort_all();
            while conns.join_next().await.is_some() {}
        });
        Ok(Self {
            listening,
            stop: Some(stop),
            task: Some(task),
        })
    }

    /// Whether the listen connection is up (the guest hands connections to the host).
    #[must_use]
    pub fn is_listening(&self) -> bool {
        *self.listening.borrow()
    }

    /// Wait until the listen connection is up, at most `timeout`. True if it is.
    pub async fn wait_listening(&self, timeout: Duration) -> bool {
        let mut rx = self.listening.clone();
        tokio::time::timeout(timeout, rx.wait_for(|l| *l))
            .await
            .is_ok_and(|r| r.is_ok())
    }

    /// Stop: close the listen connection and every relayed connection, and wait for them.
    pub async fn shutdown(mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(task) = self.task.take()
            && tokio::time::timeout(Duration::from_secs(5), task)
                .await
                .is_err()
        {
            warn!("forwards did not stop in time");
        }
    }
}

impl Drop for ForwardAcceptor {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            // Aborting the task drops its JoinSet, which aborts every relay.
            task.abort();
        }
    }
}

/// Open the listen connection, serve it until it drops, reconnect with backoff. Never
/// returns; the caller stops it.
async fn listen_loop(ctx: &Arc<Ctx>, listening: &watch::Sender<bool>, conns: &mut JoinSet<()>) {
    let mut backoff = ctx.config.reconnect_min;
    loop {
        match open_listen(ctx).await {
            Ok((stream, ports)) => {
                for p in ctx.ports.keys() {
                    if !ports.contains(p) {
                        warn!(sandbox = %ctx.sandbox, port = p, "crabd does not listen on this forward port");
                    }
                }
                backoff = ctx.config.reconnect_min;
                let _ = listening.send(true);
                let reason = serve_listen(ctx, stream, conns).await;
                let _ = listening.send(false);
                debug!(sandbox = %ctx.sandbox, %reason, "forward.listen connection ended; reconnecting");
            }
            Err(e) => {
                debug!(sandbox = %ctx.sandbox, error = %e, "cannot open forward.listen");
            }
        }
        while conns.try_join_next().is_some() {}
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(ctx.config.reconnect_max);
    }
}

async fn open_listen(ctx: &Ctx) -> Result<(BoxedStream, Vec<u16>)> {
    let handshake = async {
        let mut stream =
            ctx.connector.connect().await.map_err(|e| {
                SandboxError::Unavailable(format!("connect for forward.listen: {e}"))
            })?;
        let frame =
            encode_frame(&ForwardListenParams {}.to_request(1)).map_err(ProtocolError::from)?;
        stream
            .write_all(&frame)
            .await
            .map_err(|e| SandboxError::Transport(format!("send forward.listen: {e}")))?;
        stream
            .flush()
            .await
            .map_err(|e| SandboxError::Transport(format!("send forward.listen: {e}")))?;
        let ack =
            ForwardListenAck::from_value(read_one_response(&mut stream).await?.into_result()?)?;
        Ok((stream, ack.ports))
    };
    tokio::time::timeout(ctx.config.open_timeout, handshake)
        .await
        .map_err(|_| SandboxError::Timeout {
            what: "forward.listen".into(),
            after: ctx.config.open_timeout,
        })?
}

/// Read announcements until the connection fails; returns why. Rejections are written by
/// a writer task, so a slow target never holds up reading.
async fn serve_listen(ctx: &Arc<Ctx>, stream: BoxedStream, conns: &mut JoinSet<()>) -> String {
    let (mut reader, mut writer) = tokio::io::split(stream);
    // Rejections are best effort: an unclaimed connection is closed by the guest anyway.
    let (reject_tx, mut reject_rx) = mpsc::channel::<u64>(64);
    let write_failed = Arc::new(AtomicBool::new(false));
    let wf = Arc::clone(&write_failed);
    let mut writer_task = tokio::spawn(async move {
        while let Some(conn_id) = reject_rx.recv().await {
            let frame = encode_frame(&ForwardRejectParams { conn_id }.to_notification())
                .expect("reject frame encodes");
            if writer.write_all(&frame).await.is_err() || writer.flush().await.is_err() {
                wf.store(true, Ordering::SeqCst);
                return;
            }
        }
    });
    let reason = loop {
        while conns.try_join_next().is_some() {}
        let msg = tokio::select! {
            m = read_one_message(&mut reader) => m,
            _ = &mut writer_task, if !writer_task.is_finished() => {
                if write_failed.load(Ordering::SeqCst) {
                    break "writing forward.reject failed".to_owned();
                }
                continue;
            }
        };
        let n = match msg {
            Ok(RpcMessage::Notification(n)) if n.method == METHOD_FORWARD_INCOMING => n,
            Ok(other) => {
                debug!(
                    ?other,
                    "ignoring unexpected message on the forward.listen connection"
                );
                continue;
            }
            Err(e) => break e.to_string(),
        };
        let inc = match ForwardIncoming::from_notification(&n) {
            Ok(i) => i,
            Err(e) => break format!("malformed forward.incoming: {e}"),
        };
        match admit(ctx, inc) {
            Some(slots) => {
                let ctx = Arc::clone(ctx);
                let reject = reject_tx.clone();
                conns.spawn(async move {
                    if let Err(e) = handle(&ctx, inc, &reject).await {
                        debug!(sandbox = %ctx.sandbox, conn_id = inc.conn_id, port = inc.port, error = %e, "forwarded connection ended");
                    }
                    drop(slots);
                });
            }
            None => {
                let _ = reject_tx.try_send(inc.conn_id);
            }
        }
    };
    writer_task.abort();
    reason
}

/// The two slots a relayed connection holds.
type Slots = (
    tokio::sync::OwnedSemaphorePermit,
    tokio::sync::OwnedSemaphorePermit,
);

/// Take a per-forward and a per-sandbox slot, or `None` (refuse).
fn admit(ctx: &Ctx, inc: ForwardIncoming) -> Option<Slots> {
    let Some(port) = ctx.ports.get(&inc.port) else {
        warn!(sandbox = %ctx.sandbox, port = inc.port, "crabd announced a port that is not a forward; refusing");
        return None;
    };
    let Ok(per_port) = Arc::clone(port).try_acquire_owned() else {
        debug!(sandbox = %ctx.sandbox, port = inc.port, "forward connection cap reached; refusing");
        return None;
    };
    let Ok(total) = Arc::clone(&ctx.total).try_acquire_owned() else {
        debug!(sandbox = %ctx.sandbox, "sandbox forward connection cap reached; refusing");
        return None;
    };
    Some((per_port, total))
}

/// One announced connection: target, accept, relay.
async fn handle(ctx: &Ctx, inc: ForwardIncoming, reject: &mpsc::Sender<u64>) -> Result<()> {
    let target = match tokio::time::timeout(
        ctx.config.target_timeout,
        ctx.target.connect(ctx.sandbox, inc.port),
    )
    .await
    {
        Ok(Ok(t)) => t,
        Ok(Err(e)) => {
            let _ = reject.send(inc.conn_id).await;
            return Err(SandboxError::Transport(format!("target refused: {e}")));
        }
        Err(_) => {
            let _ = reject.send(inc.conn_id).await;
            return Err(SandboxError::Timeout {
                what: "forward target".into(),
                after: ctx.config.target_timeout,
            });
        }
    };
    let guest = match open_accept(ctx, inc).await {
        Ok(g) => g,
        Err(e) => {
            // The guest may still hold the connection: tell it, so it closes it now.
            let _ = reject.send(inc.conn_id).await;
            return Err(e);
        }
    };
    relay(target, guest)
        .await
        .map(|_| ())
        .map_err(SandboxError::Io)
}

async fn open_accept(ctx: &Ctx, inc: ForwardIncoming) -> Result<BoxedStream> {
    let handshake = async {
        let mut stream =
            ctx.connector.connect().await.map_err(|e| {
                SandboxError::Unavailable(format!("connect for forward.accept: {e}"))
            })?;
        let frame = encode_frame(
            &ForwardAcceptParams {
                conn_id: inc.conn_id,
            }
            .to_request(1),
        )
        .map_err(ProtocolError::from)?;
        stream
            .write_all(&frame)
            .await
            .map_err(|e| SandboxError::Transport(format!("send forward.accept: {e}")))?;
        stream
            .flush()
            .await
            .map_err(|e| SandboxError::Transport(format!("send forward.accept: {e}")))?;
        let ack =
            ForwardAcceptAck::from_value(read_one_response(&mut stream).await?.into_result()?)?;
        if ack.port != inc.port {
            return Err(SandboxError::Transport(format!(
                "crabd accepted a connection to port {} that was announced for port {}",
                ack.port, inc.port
            )));
        }
        Ok(stream)
    };
    tokio::time::timeout(ctx.config.open_timeout, handshake)
        .await
        .map_err(|_| SandboxError::Timeout {
            what: "forward.accept".into(),
            after: ctx.config.open_timeout,
        })?
}

/// Relay between the target (raw bytes) and the guest connection (records) until both
/// directions ended. An error ends both. Returns the bytes sent to and from the guest.
pub(crate) async fn relay<T, G>(target: T, guest: G) -> io::Result<(u64, u64)>
where
    T: AsyncRead + AsyncWrite + Unpin,
    G: AsyncRead + AsyncWrite + Unpin,
{
    let (mut target_r, mut target_w) = tokio::io::split(target);
    let (mut guest_r, mut guest_w) = tokio::io::split(guest);
    let to_guest = async {
        let mut buf = vec![0u8; MAX_RECORD_DATA_LEN];
        let mut total = 0u64;
        loop {
            let n = target_r.read(&mut buf).await?;
            let header = if n == 0 {
                encode_record_header(RecordKind::Eof, 0)
            } else {
                encode_record_header(RecordKind::Data, n)
            }
            .expect("within the record limit");
            guest_w.write_all(&header).await?;
            guest_w.write_all(&buf[..n]).await?;
            guest_w.flush().await?;
            if n == 0 {
                // Never shut the transport down: the EOF record is the half-close.
                return Ok::<_, io::Error>(total);
            }
            total += n as u64;
        }
    };
    let from_guest = async {
        let mut payload = vec![0u8; MAX_RECORD_DATA_LEN];
        let mut total = 0u64;
        loop {
            let mut header = [0u8; RECORD_HEADER_LEN];
            guest_r.read_exact(&mut header).await?;
            let (kind, len) = parse_record_header(RecordChannel::Forward, header)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            guest_r.read_exact(&mut payload[..len]).await?;
            match kind {
                RecordKind::Data => {
                    target_w.write_all(&payload[..len]).await?;
                    target_w.flush().await?;
                    total += len as u64;
                }
                _ => {
                    target_w.shutdown().await?;
                    return Ok::<_, io::Error>(total);
                }
            }
        }
    };
    tokio::try_join!(to_guest, from_guest)
}

#[cfg(test)]
mod tests;
