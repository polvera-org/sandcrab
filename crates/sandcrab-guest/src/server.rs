//! The `crabd` RPC server: listeners, per-connection multiplexing, graceful shutdown.
//!
//! # Connection model
//!
//! Each accepted connection is split into a **reader** (this task) and a single **writer**
//! task fed by an mpsc channel. Every request frame is handled in its own task, so slow
//! tools do not block fast ones on the same connection; responses are written as they
//! complete (possibly out of order) and the host matches them by `id`. At most
//! [`ServerConfig::max_inflight_per_connection`] requests run at once per connection; the
//! reader stops reading until a slot frees up (backpressure).
//!
//! Errors:
//! - invalid JSON → `-32700` (id `null`), connection stays open (frame boundaries intact);
//! - valid JSON that is not a JSON-RPC request → `-32600` (id sniffed when possible);
//! - unknown method → `-32601`; bad params → `-32602` with the serde message;
//! - bad header (empty or `> MAX_FRAME_LEN`) → error response (id `null`), then close: the
//!   stream can no longer be trusted to be frame-aligned;
//! - a panicking handler → `-32603` for that request only.
//!
//! Inbound notifications and responses are ignored (Phase 1 hosts send neither).
//!
//! # Tunnels
//!
//! A connection whose **first** frame is `tunnel.open` leaves frame mode: after the ack it
//! is a raw byte relay to a guest loopback port ([`crate::tunnel`]). Tunnel connections give
//! back their `max_connections` slot and are bounded separately by
//! [`TunnelConfig::max_tunnels`], so port forwards cannot starve tool connections (nor the
//! reverse). `tunnel.open` on a connection that already carried a frame is `-32600`.
//!
//! # Processes
//!
//! A connection whose **first** frame is `process.open` is handed to a `crabd supervise`
//! that runs one program and then carries records ([`crate::supervise`]). It gives back its
//! `max_connections` slot at the handover; [`crate::supervise::SupervisorConfig::max_processes`]
//! bounds them instead. `process.open` on a connection that already carried a frame is
//! `-32600`. `process.cancel` and `fs.identity` are ordinary methods.
//!
//! # Shutdown
//!
//! [`Server::run`] takes a shutdown future (normally [`shutdown_signal`]). When it fires the
//! listener closes, connections stop reading new frames, in-flight requests get up to
//! [`ServerConfig::shutdown_grace`] to finish and flush their responses, then everything
//! left is aborted, background processes are killed and the Unix socket file is removed.
//!
//! # Authentication
//!
//! With [`ServerConfig::auth`] set, every connection must open with a valid `auth` frame
//! ([`crate::auth`]) before anything else, including `tunnel.open`; otherwise it gets one
//! `UNAUTHORIZED` frame and is closed. Without it (microVM vsock, local tests) connections
//! are served as before.
//!
//! An unauthenticated connection does **not** hold one of the `max_connections` slots: it
//! waits in a separate, small [`PendingQueue`] ([`ServerConfig::max_pending`], oldest evicted
//! when full) and only takes a slot once it has authenticated. So peers without the token
//! cannot starve the real client by holding idle connections ([`crate::preauth`]). Refusals
//! are logged at most once a second.
//!
//! # Transports
//!
//! - Unix socket (Docker, local tests): a stale socket at the path is removed on start
//!   (a *live* one, or a non-socket file, is an error). The socket is chmod `0660`: the
//!   Docker backend runs `crabd` as the host user's uid and the host connects as that same
//!   uid through a bind-mounted directory, so owner (+ group) access is all that is needed.
//! - TCP: set [`ServerConfig::auth`] for anything but a private test listener (bind logs a
//!   warning when it is unauthenticated).
//! - vsock (Firecracker, Linux only): `VMADDR_CID_ANY:<port>` via [`crate::vsock`]. The
//!   only peer is the VMM's host-side socket, so there is no extra authentication.

use std::{
    future::Future,
    io,
    net::SocketAddr,
    os::{
        fd::AsFd,
        unix::fs::{FileTypeExt, MetadataExt, PermissionsExt},
    },
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
    time::Duration,
};

use sandcrab_protocol::{
    Call, FileIdentity, FrameError, FsIdentityParams, FsIdentityResult, HelloParams, HelloResult,
    METHOD_PROCESS_OPEN, METHOD_TUNNEL_OPEN, PROTOCOL_VERSION, PingResult, ProtocolError, Reply,
    RpcMessage, RpcRequest, RpcResponse, TOOL_NAMES, encode_frame, features,
};
use tokio::{
    io::{AsyncRead, AsyncWrite, AsyncWriteExt},
    net::{TcpListener, TcpStream, UnixListener, UnixStream},
    sync::{OwnedSemaphorePermit, Semaphore, mpsc, watch},
    task::JoinSet,
};
use tracing::{debug, error, info, warn};

use crate::{
    CRABD_VERSION, ToolContext,
    auth::{self, AuthConfig},
    codec,
    preauth::{PendingQueue, PendingTicket, RateLimitedLog},
    supervise::{self, SupervisorConfig},
    tools,
    tunnel::{self, TunnelConfig, TunnelService},
};

/// Where the server listens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListenAddr {
    /// Unix domain socket path.
    Unix(PathBuf),
    /// TCP `host:port` (port `0` picks a free port; see [`Server::tcp_addr`]).
    Tcp(String),
    /// virtio-vsock port (Linux only).
    Vsock(u32),
}

/// Server tuning.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub listen: ListenAddr,
    /// Max concurrently served connections; further clients wait in the accept backlog.
    pub max_connections: usize,
    /// Max concurrently running requests per connection.
    pub max_inflight_per_connection: usize,
    /// How long in-flight requests may run after shutdown starts.
    pub shutdown_grace: Duration,
    /// `tunnel.open` limits.
    pub tunnels: TunnelConfig,
    /// Require the `auth` frame on every connection (see [`crate::auth`]).
    pub auth: Option<AuthConfig>,
    /// With `auth`: max connections authenticating at once (outside `max_connections`).
    pub max_pending: usize,
    /// `process.open`: the supervisor binary and `--max-processes`.
    pub supervisor: SupervisorConfig,
}

impl ServerConfig {
    #[must_use]
    pub fn new(listen: ListenAddr) -> Self {
        Self {
            listen,
            max_connections: 64,
            max_inflight_per_connection: 64,
            shutdown_grace: Duration::from_secs(10),
            tunnels: TunnelConfig::default(),
            auth: None,
            max_pending: crate::preauth::DEFAULT_PENDING,
            supervisor: SupervisorConfig::default(),
        }
    }
}

/// Boxed future returned by a [`Handler`].
pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

/// Turns one request into its response. The server uses [`tool_handler`]; tests can inject
/// their own to exercise the connection machinery.
pub type Handler = Arc<dyn Fn(RpcRequest) -> BoxFuture<RpcResponse> + Send + Sync>;

/// The production handler: `hello` / `ping` here, everything else via [`tools::dispatch`].
#[must_use]
pub fn tool_handler(ctx: Arc<ToolContext>) -> Handler {
    Arc::new(move |req| {
        let ctx = Arc::clone(&ctx);
        Box::pin(async move { handle_request(&ctx, req).await })
    })
}

/// Handle one request against `ctx`.
pub async fn handle_request(ctx: &ToolContext, req: RpcRequest) -> RpcResponse {
    let outcome = match Call::from_request(&req) {
        Ok(Call::Hello(p)) => hello(ctx, &p).map(Reply::Hello),
        Ok(Call::Ping) => Ok(Reply::Ping(PingResult {})),
        Ok(Call::ProcessCancel(p)) => Ok(Reply::ProcessCancel(ctx.supervisors.cancel(&p).await)),
        Ok(Call::FsIdentity(p)) => fs_identity(&p).map(Reply::FsIdentity),
        // Protocol 0.3 methods this crabd does not serve yet.
        Ok(other @ Call::ForwardStats) => {
            Err(ProtocolError::MethodNotFound(other.method().to_owned()))
        }
        Ok(Call::Tool(call)) => {
            let tool = call.name();
            let res = tools::dispatch(ctx, call).await;
            if let Err(e) = &res {
                debug!(id = %req.id, tool, error = %e, "tool call failed");
            }
            res.map(Reply::Tool)
        }
        Err(e) => Err(e),
    };
    RpcResponse::from_outcome(req.id, outcome)
}

fn hello(ctx: &ToolContext, p: &HelloParams) -> Result<HelloResult, ProtocolError> {
    if let Err(e) = p.check_version() {
        warn!(error = %e, "hello refused");
        return Err(e);
    }
    info!(client = p.client.as_deref().unwrap_or("-"), "hello");
    Ok(HelloResult {
        crabd_version: CRABD_VERSION.to_owned(),
        protocol_version: PROTOCOL_VERSION.to_owned(),
        tools: TOOL_NAMES.iter().map(|&t| t.to_owned()).collect(),
        default_cwd: ctx.default_cwd.display().to_string(),
        features: hello_features(),
        forward_ports: Vec::new(),
    })
}

/// What this `crabd` serves beyond the tools. The tree kill follows every descendant on Linux
/// (the supervisor is a child subreaper); elsewhere it reaches the process group only.
#[must_use]
pub fn hello_features() -> Vec<String> {
    let tree_kill = if cfg!(target_os = "linux") {
        features::TREE_KILL_SUBREAPER
    } else {
        features::TREE_KILL_PROCESS_GROUP
    };
    vec![features::PROCESS_OPEN.to_owned(), tree_kill.to_owned()]
}

/// `fs.identity`: `(dev, ino)` of each path, following symlinks like `stat`.
fn fs_identity(p: &FsIdentityParams) -> Result<FsIdentityResult, ProtocolError> {
    p.paths
        .iter()
        .map(|path| match std::fs::metadata(path) {
            Ok(md) => Ok(FileIdentity {
                dev: md.dev(),
                ino: md.ino(),
            }),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                Err(ProtocolError::NotFound(path.clone()))
            }
            Err(e) => Err(ProtocolError::ExecutionFailed(format!("stat {path}: {e}"))),
        })
        .collect::<Result<_, _>>()
        .map(FsIdentityResult)
}

/// Resolves on the first SIGTERM or SIGINT. Handlers are installed when this is called,
/// so install errors surface before the server starts.
pub fn shutdown_signal() -> io::Result<impl Future<Output = ()> + Send> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate())?;
    let mut int = signal(SignalKind::interrupt())?;
    Ok(async move {
        tokio::select! {
            _ = term.recv() => info!("SIGTERM received"),
            _ = int.recv() => info!("SIGINT received"),
        }
    })
}

enum Listener {
    Unix {
        listener: UnixListener,
        path: PathBuf,
    },
    Tcp(TcpListener),
    #[cfg(target_os = "linux")]
    Vsock(crate::vsock::VsockListener, u32),
}

enum Conn {
    Unix(UnixStream),
    Tcp(TcpStream, SocketAddr),
    #[cfg(target_os = "linux")]
    Vsock(crate::vsock::VsockStream, String),
}

impl Conn {
    fn peer(&self) -> &str {
        match self {
            Self::Unix(_) => "unix",
            Self::Tcp(..) => "tcp",
            #[cfg(target_os = "linux")]
            Self::Vsock(_, p) => p,
        }
    }
}

impl Listener {
    fn is_vsock(&self) -> bool {
        #[cfg(target_os = "linux")]
        return matches!(self, Self::Vsock(..));
        #[cfg(not(target_os = "linux"))]
        false
    }

    async fn accept(&self) -> io::Result<Conn> {
        match self {
            Self::Unix { listener, .. } => listener.accept().await.map(|(s, _)| Conn::Unix(s)),
            Self::Tcp(listener) => {
                let (s, peer) = listener.accept().await?;
                s.set_nodelay(true)?;
                Ok(Conn::Tcp(s, peer))
            }
            #[cfg(target_os = "linux")]
            Self::Vsock(listener, _) => {
                let (s, peer) = listener.accept().await?;
                Ok(Conn::Vsock(
                    s,
                    format!("vsock:{}:{}", peer.cid(), peer.port()),
                ))
            }
        }
    }
}

/// A bound, not yet running server.
pub struct Server {
    listener: Listener,
    ctx: Arc<ToolContext>,
    config: ServerConfig,
}

impl Server {
    /// Bind the configured transport.
    pub async fn bind(config: ServerConfig, ctx: Arc<ToolContext>) -> io::Result<Self> {
        if config.max_connections == 0
            || config.max_inflight_per_connection == 0
            || config.tunnels.max_tunnels == 0
            || config.max_pending == 0
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "max_connections, max_inflight_per_connection, max_tunnels and max_pending must \
                 be at least 1",
            ));
        }
        let listener = match &config.listen {
            ListenAddr::Unix(path) => Listener::Unix {
                listener: bind_unix(path)?,
                path: path.clone(),
            },
            ListenAddr::Tcp(addr) => {
                let l = TcpListener::bind(addr).await?;
                if config.auth.is_none() {
                    warn!(
                        addr = %l.local_addr()?,
                        "TCP transport is unauthenticated; bind only to loopback or a private network"
                    );
                }
                Listener::Tcp(l)
            }
            #[cfg(target_os = "linux")]
            ListenAddr::Vsock(port) => Listener::Vsock(crate::vsock::bind(*port)?, *port),
            #[cfg(not(target_os = "linux"))]
            ListenAddr::Vsock(port) => return Err(crate::vsock::unsupported(*port)),
        };
        Ok(Self {
            listener,
            ctx,
            config,
        })
    }

    /// Bound TCP address (useful with port `0`), `None` for other transports.
    #[must_use]
    pub fn tcp_addr(&self) -> Option<SocketAddr> {
        match &self.listener {
            Listener::Tcp(l) => l.local_addr().ok(),
            _ => None,
        }
    }

    /// Human-readable endpoint, e.g. `unix:/run/crabd.sock` or `tcp:127.0.0.1:5000`.
    #[must_use]
    pub fn endpoint(&self) -> String {
        match &self.listener {
            Listener::Unix { path, .. } => format!("unix:{}", path.display()),
            Listener::Tcp(l) => l
                .local_addr()
                .map_or_else(|_| "tcp:?".to_owned(), |a| format!("tcp:{a}")),
            #[cfg(target_os = "linux")]
            Listener::Vsock(_, port) => format!("vsock:{port}"),
        }
    }

    /// Serve until `shutdown` resolves, then drain (see module docs).
    pub async fn run(self, shutdown: impl Future<Output = ()>) -> io::Result<()> {
        let Self {
            listener,
            ctx,
            config,
        } = self;
        let handler = tool_handler(Arc::clone(&ctx));
        let supervisor = Arc::new(config.supervisor.clone());
        // vsock bridges drop half-close: end tunnels on target EOF (see `crate::tunnel`).
        let tunnel_config = TunnelConfig {
            close_on_target_eof: config.tunnels.close_on_target_eof || listener.is_vsock(),
            ..config.tunnels
        };
        let tunnels = Arc::new(TunnelService::new(tunnel_config));
        let auth = config.auth.clone().map(Arc::new);
        let (stop_tx, stop_rx) = watch::channel(false);
        let slots = Arc::new(Semaphore::new(config.max_connections));
        let pending = PendingQueue::new(config.max_pending);
        let refusals = Arc::new(RateLimitedLog::new(Duration::from_secs(1)));
        let mut conns = JoinSet::new();
        tokio::pin!(shutdown);
        info!(
            default_cwd = %ctx.default_cwd.display(),
            max_connections = config.max_connections,
            authenticated = auth.is_some(),
            "crabd serving"
        );

        loop {
            while conns.try_join_next().is_some() {}
            // Unauthenticated transports take their slot before accepting (backpressure via
            // the accept backlog). Authenticated ones accept at once and take it after `auth`.
            let permit = if auth.is_none() {
                Some(tokio::select! {
                    () = &mut shutdown => break,
                    p = Arc::clone(&slots).acquire_owned() => p.expect("connection semaphore is never closed"),
                })
            } else {
                None
            };
            let accepted = tokio::select! {
                () = &mut shutdown => break,
                c = listener.accept() => c,
            };
            let conn = match accepted {
                Ok(c) => c,
                Err(e) => {
                    // EMFILE / ECONNABORTED etc. are transient; back off instead of spinning.
                    warn!(error = %e, "accept failed");
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                }
            };
            let permit = match permit {
                Some(p) => ConnSlot::Held(p),
                None => {
                    let (ticket, evicted) = pending.admit();
                    if evicted {
                        refusals.warn(
                            conn.peer(),
                            "pending connections full; evicted the oldest unauthenticated one",
                        );
                    }
                    ConnSlot::AfterAuth {
                        ticket,
                        slots: Arc::clone(&slots),
                    }
                }
            };
            let handler = Arc::clone(&handler);
            let stop = stop_rx.clone();
            let inflight = config.max_inflight_per_connection;
            let conn_cfg = ConnConfig {
                handler,
                max_inflight: inflight,
                tunnels: Arc::clone(&tunnels),
                auth: auth.clone(),
                refusals: Arc::clone(&refusals),
                ctx: Arc::clone(&ctx),
                supervisor: Arc::clone(&supervisor),
            };
            match conn {
                Conn::Unix(s) => conns.spawn(async move {
                    serve_stream(s, conn_cfg, stop, "unix".into(), permit).await;
                }),
                Conn::Tcp(s, peer) => conns.spawn(async move {
                    serve_stream(s, conn_cfg, stop, peer.to_string(), permit).await;
                }),
                #[cfg(target_os = "linux")]
                Conn::Vsock(s, peer) => conns.spawn(async move {
                    serve_stream(s, conn_cfg, stop, peer, permit).await;
                }),
            };
        }

        info!(connections = conns.len(), "shutting down");
        drop(listener);
        let _ = stop_tx.send(true);
        let drained = tokio::time::timeout(config.shutdown_grace, async {
            while conns.join_next().await.is_some() {}
        })
        .await;
        if drained.is_err() {
            warn!(
                grace_ms = config.shutdown_grace.as_millis(),
                connections = conns.len(),
                "in-flight requests did not finish in time; aborting"
            );
            conns.abort_all();
            while conns.join_next().await.is_some() {}
        }
        ctx.processes.shutdown().await;
        ctx.supervisors.shutdown(config.shutdown_grace).await;
        if let ListenAddr::Unix(path) = &config.listen {
            match std::fs::remove_file(path) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => warn!(path = %path.display(), error = %e, "failed to remove socket"),
            }
        }
        info!("crabd stopped");
        Ok(())
    }
}

fn bind_unix(path: &Path) -> io::Result<UnixListener> {
    match std::fs::symlink_metadata(path) {
        Ok(md) if md.file_type().is_socket() => {
            if std::os::unix::net::UnixStream::connect(path).is_ok() {
                return Err(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    format!("{} is in use by a live server", path.display()),
                ));
            }
            std::fs::remove_file(path)?;
            info!(path = %path.display(), "removed stale socket");
        }
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!(
                    "{} exists and is not a socket; refusing to replace it",
                    path.display()
                ),
            ));
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o660))?;
    Ok(listener)
}

/// Per-connection settings shared by every accepted connection of a [`Server`].
#[derive(Clone)]
pub struct ConnConfig {
    pub handler: Handler,
    pub max_inflight: usize,
    pub tunnels: Arc<TunnelService>,
    /// `Some` = the connection must authenticate first.
    pub auth: Option<Arc<AuthConfig>>,
    /// Rate-limited log for refused connections.
    pub refusals: Arc<RateLimitedLog>,
    /// For `process.open` connections.
    pub ctx: Arc<ToolContext>,
    pub supervisor: Arc<SupervisorConfig>,
}

/// A connection's claim on [`ServerConfig::max_connections`].
pub enum ConnSlot {
    /// Already holds its slot (unauthenticated transports).
    Held(OwnedSemaphorePermit),
    /// Waits in the pre-auth queue; takes a slot from `slots` once authenticated.
    AfterAuth {
        ticket: PendingTicket,
        slots: Arc<Semaphore>,
    },
}

/// Serve an accepted connection: authenticate it (when configured), then a raw tunnel if
/// its first frame is `tunnel.open`, a supervised process if it is `process.open`, else frame
/// mode ([`serve_connection`]). `slot` is the
/// claim on a `max_connections` permit (taken only after `auth`, see [`ConnSlot`]); a tunnel
/// releases the permit once it has its own slot.
pub async fn serve_stream<S>(
    mut stream: S,
    cfg: ConnConfig,
    mut stop: watch::Receiver<bool>,
    peer: String,
    slot: ConnSlot,
) where
    S: AsyncRead + AsyncWrite + AsFd + Unpin + Send + 'static,
{
    let ConnConfig {
        handler,
        max_inflight,
        tunnels,
        auth,
        refusals,
        ctx,
        supervisor,
    } = cfg;
    debug!(%peer, "connection opened");
    let mut slot = slot;
    if let Some(auth) = auth {
        // The pending ticket bounds the whole exchange (auth deadline + verdict write).
        let deadline = auth.timeout + Duration::from_secs(1);
        let verdict = match &mut slot {
            ConnSlot::AfterAuth { ticket, .. } => tokio::select! {
                biased;
                _ = stop.wait_for(|s| *s) => return,
                v = ticket.run(deadline, auth::authenticate(&mut stream, &auth)) => v,
            },
            ConnSlot::Held(_) => tokio::select! {
                biased;
                _ = stop.wait_for(|s| *s) => return,
                v = auth::authenticate(&mut stream, &auth) => Ok(v),
            },
        };
        match verdict {
            Ok(Ok(())) => debug!(%peer, "connection authenticated"),
            Ok(Err(why)) => {
                refusals.warn(
                    &peer,
                    &format!("connection failed authentication ({why:?}); closed"),
                );
                return;
            }
            Err(end) => {
                refusals.warn(
                    &peer,
                    &format!("unauthenticated connection dropped ({end:?})"),
                );
                return;
            }
        }
    }
    let conn_slot = match slot {
        ConnSlot::Held(p) => p,
        ConnSlot::AfterAuth { ticket, slots } => {
            drop(ticket);
            tokio::select! {
                biased;
                _ = stop.wait_for(|s| *s) => return,
                p = slots.acquire_owned() => p.expect("connection semaphore is never closed"),
            }
        }
    };
    let first = tokio::select! {
        biased;
        _ = stop.wait_for(|s| *s) => return,
        f = codec::read_frame(&mut stream) => f,
    };
    if let Ok(Some(payload)) = &first
        && let Some(req) = tunnel::as_tunnel_open(payload)
    {
        drop(conn_slot);
        tunnels.serve(stream, req, stop, &peer).await;
        debug!(%peer, "tunnel connection closed");
        return;
    }
    if let Ok(Some(payload)) = &first
        && let Some(req) = as_process_open(payload)
    {
        drop(conn_slot);
        supervise::serve_open(stream, req, ctx, &supervisor, &peer).await;
        return;
    }
    let (rd, wr) = tokio::io::split(stream);
    serve_frames(rd, wr, Some(first), handler, max_inflight, stop, peer).await;
    drop(conn_slot);
}

/// Serve one connection in frame mode until EOF, a fatal framing error, or `stop` turns
/// `true`; then wait for in-flight requests and flush their responses. No tunnels.
pub async fn serve_connection<S>(
    stream: S,
    handler: Handler,
    max_inflight: usize,
    stop: watch::Receiver<bool>,
    peer: String,
) where
    S: AsyncRead + AsyncWrite + Send + 'static,
{
    debug!(%peer, "connection opened");
    let (rd, wr) = tokio::io::split(stream);
    serve_frames(rd, wr, None, handler, max_inflight, stop, peer).await;
}

type FirstFrame = Result<Option<Vec<u8>>, FrameError>;

async fn serve_frames<R, W>(
    mut rd: R,
    wr: W,
    mut first: Option<FirstFrame>,
    handler: Handler,
    max_inflight: usize,
    mut stop: watch::Receiver<bool>,
    peer: String,
) where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (tx, rx) = mpsc::channel::<RpcResponse>(max_inflight + 4);
    let writer = tokio::spawn(write_loop(wr, rx, peer.clone()));
    let slots = Arc::new(Semaphore::new(max_inflight));
    let mut tasks = JoinSet::new();

    loop {
        while tasks.try_join_next().is_some() {}
        let permit = tokio::select! {
            biased;
            _ = stop.wait_for(|s| *s) => break,
            p = Arc::clone(&slots).acquire_owned() => p.expect("request semaphore is never closed"),
        };
        // Cancelling `read_frame` loses a partial frame; fine, we are closing anyway.
        let frame = if let Some(f) = first.take() {
            f
        } else {
            tokio::select! {
                biased;
                _ = stop.wait_for(|s| *s) => break,
                f = codec::read_frame(&mut rd) => f,
            }
        };
        let payload = match frame {
            Ok(Some(p)) => p,
            Ok(None) => break,
            Err(e @ (FrameError::Io(_) | FrameError::Truncated { .. })) => {
                debug!(%peer, error = %e, "connection read ended");
                break;
            }
            Err(e) => {
                warn!(%peer, error = %e, "unrecoverable framing error; closing connection");
                let _ = tx.send(RpcResponse::failure(None, &e.into())).await;
                break;
            }
        };
        match RpcMessage::from_slice(&payload) {
            Ok(RpcMessage::Request(req))
                if req.method == METHOD_TUNNEL_OPEN || req.method == METHOD_PROCESS_OPEN =>
            {
                let e = ProtocolError::InvalidRequest(format!(
                    "`{}` must be the first frame of a fresh connection",
                    req.method
                ));
                let _ = tx.send(RpcResponse::failure(Some(req.id), &e)).await;
                drop(permit);
            }
            Ok(RpcMessage::Request(req)) => {
                let handler = Arc::clone(&handler);
                let tx = tx.clone();
                tasks.spawn(async move {
                    let id = req.id.clone();
                    let method = req.method.clone();
                    // Inner task so a panicking tool still yields a response for its id.
                    let resp = match tokio::spawn(handler(req)).await {
                        Ok(resp) => resp,
                        Err(e) => {
                            error!(%id, %method, error = %e, "request handler crashed");
                            RpcResponse::failure(
                                Some(id),
                                &ProtocolError::Internal(format!("`{method}` handler crashed")),
                            )
                        }
                    };
                    let _ = tx.send(resp).await;
                    drop(permit);
                });
            }
            Ok(RpcMessage::Notification(n)) => {
                debug!(%peer, method = %n.method, "ignoring notification");
            }
            Ok(RpcMessage::Response(r)) => {
                debug!(%peer, id = ?r.id, "ignoring unexpected response");
            }
            Err(e) => {
                let id = match e {
                    ProtocolError::Parse(_) => None,
                    _ => RpcMessage::sniff_id(&payload),
                };
                debug!(%peer, error = %e, "bad message");
                let _ = tx.send(RpcResponse::failure(id, &e)).await;
            }
        }
    }

    while tasks.join_next().await.is_some() {}
    drop(tx);
    let _ = writer.await;
    debug!(%peer, "connection closed");
}

/// The request if `payload` is a `process.open` request (params are checked later).
fn as_process_open(payload: &[u8]) -> Option<RpcRequest> {
    match RpcMessage::from_slice(payload) {
        Ok(RpcMessage::Request(req)) if req.method == METHOD_PROCESS_OPEN => Some(req),
        _ => None,
    }
}

async fn write_loop<W>(mut wr: W, mut rx: mpsc::Receiver<RpcResponse>, peer: String)
where
    W: AsyncWrite + Unpin,
{
    while let Some(resp) = rx.recv().await {
        let frame = match encode_frame(&resp) {
            Ok(f) => f,
            Err(e) => {
                // Result too large for one frame: replace it with an error for the same id.
                warn!(%peer, id = ?resp.id, error = %e, "response not encodable");
                match encode_frame(&RpcResponse::failure(resp.id, &e.into())) {
                    Ok(f) => f,
                    Err(e) => {
                        error!(%peer, error = %e, "error response not encodable");
                        continue;
                    }
                }
            }
        };
        if let Err(e) = async {
            wr.write_all(&frame).await?;
            wr.flush().await
        }
        .await
        {
            debug!(%peer, error = %e, "write failed; dropping remaining responses");
            break;
        }
    }
    let _ = wr.shutdown().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use sandcrab_protocol::{RequestId, ResponsePayload};
    use serde_json::json;
    use tokio::io::DuplexStream;

    /// Handler where method `sleep` waits `params.ms`, `panic` panics, anything else echoes.
    fn test_handler() -> Handler {
        Arc::new(|req: RpcRequest| {
            Box::pin(async move {
                match req.method.as_str() {
                    "sleep" => {
                        let ms = req.params.as_ref().and_then(|p| p["ms"].as_u64()).unwrap();
                        tokio::time::sleep(Duration::from_millis(ms)).await;
                    }
                    "panic" => panic!("boom"),
                    _ => {}
                }
                RpcResponse::success(req.id, json!({ "method": req.method }))
            })
        })
    }

    fn start(stop: watch::Receiver<bool>) -> (DuplexStream, tokio::task::JoinHandle<()>) {
        let (client, server) = tokio::io::duplex(64 * 1024);
        let h = tokio::spawn(serve_connection(
            server,
            test_handler(),
            8,
            stop,
            "test".into(),
        ));
        (client, h)
    }

    async fn recv(c: &mut DuplexStream) -> RpcResponse {
        let p = codec::read_frame(c).await.unwrap().unwrap();
        serde_json::from_slice(&p).unwrap()
    }

    #[tokio::test]
    async fn responses_are_out_of_order_and_matched_by_id() {
        let (_tx, rx) = watch::channel(false);
        let (mut c, _h) = start(rx);
        let slow = RpcRequest::new(1, "sleep", Some(json!({ "ms": 300 })));
        let fast = RpcRequest::new(2, "echo", None);
        codec::write_frame(&mut c, &slow).await.unwrap();
        codec::write_frame(&mut c, &fast).await.unwrap();
        assert_eq!(recv(&mut c).await.id, Some(RequestId::Number(2)));
        assert_eq!(recv(&mut c).await.id, Some(RequestId::Number(1)));
    }

    #[tokio::test]
    async fn panicking_handler_yields_internal_error() {
        let (_tx, rx) = watch::channel(false);
        let (mut c, _h) = start(rx);
        codec::write_frame(&mut c, &RpcRequest::new("p", "panic", None))
            .await
            .unwrap();
        let resp = recv(&mut c).await;
        assert_eq!(resp.id, Some(RequestId::from("p")));
        let ResponsePayload::Error(e) = resp.payload else {
            panic!("expected error")
        };
        assert_eq!(e.code, sandcrab_protocol::codes::INTERNAL_ERROR);
        // Connection survives.
        codec::write_frame(&mut c, &RpcRequest::new(3, "echo", None))
            .await
            .unwrap();
        assert_eq!(recv(&mut c).await.id, Some(RequestId::Number(3)));
    }

    #[tokio::test]
    async fn stop_finishes_in_flight_then_closes() {
        let (stop_tx, rx) = watch::channel(false);
        let (mut c, h) = start(rx);
        codec::write_frame(
            &mut c,
            &RpcRequest::new(1, "sleep", Some(json!({ "ms": 200 }))),
        )
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        stop_tx.send(true).unwrap();
        assert_eq!(recv(&mut c).await.id, Some(RequestId::Number(1)));
        assert!(codec::read_frame(&mut c).await.unwrap().is_none());
        h.await.unwrap();
    }

    #[tokio::test]
    async fn eof_still_flushes_in_flight() {
        let (_tx, rx) = watch::channel(false);
        let (mut c, h) = start(rx);
        codec::write_frame(
            &mut c,
            &RpcRequest::new(1, "sleep", Some(json!({ "ms": 100 }))),
        )
        .await
        .unwrap();
        c.shutdown().await.unwrap();
        assert_eq!(recv(&mut c).await.id, Some(RequestId::Number(1)));
        h.await.unwrap();
    }
}
