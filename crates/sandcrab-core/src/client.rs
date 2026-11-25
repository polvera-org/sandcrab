//! Multiplexed async client for `crabd`.
//!
//! [`CrabdClient`] wraps any `AsyncRead + AsyncWrite + Unpin + Send` byte stream (Unix
//! socket, TCP, Firecracker vsock-over-UDS, tokio duplex in tests) speaking the
//! `sandcrab-protocol` framing. Many calls may be in flight at once: a writer task
//! serializes outgoing frames, a reader task routes responses to waiting callers by
//! JSON-RPC id. Every call has a timeout; a dropped (cancelled) call never desynchronizes
//! the stream.

use std::collections::HashMap;
use std::fmt;
use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use sandcrab_protocol::{
    Call, HEADER_LEN, HelloParams, HelloResult, METHOD_HELLO, PROTOCOL_VERSION, ProtocolError,
    Reply, RequestId, RpcMessage, RpcResponse, ToolCall, ToolResult, TunnelOpenAck,
    TunnelOpenParams, encode_frame, parse_header,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::{Result, SandboxError};

/// Default per-call timeout (covers the default 120 s `bash` timeout plus slack).
pub const DEFAULT_CALL_TIMEOUT: Duration = Duration::from_secs(150);
/// Slack added on top of an explicit `bash.timeout_ms` so crabd can report the timeout itself.
pub const CALL_TIMEOUT_GRACE: Duration = Duration::from_secs(10);

/// Object-safe byte stream usable as a crabd transport.
pub trait AsyncStream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> AsyncStream for T {}

pub type BoxedStream = Box<dyn AsyncStream>;

/// Opens a fresh transport stream to a sandbox's `crabd`. Backends return one from
/// `SandboxHandle::connector`; Firecracker implements it with its vsock `CONNECT` handshake.
#[async_trait]
pub trait Connector: Send + Sync + fmt::Debug {
    async fn connect(&self) -> io::Result<BoxedStream>;
}

/// Plain socket endpoints.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "addr", rename_all = "snake_case")]
pub enum Endpoint {
    #[cfg(unix)]
    Unix(PathBuf),
    Tcp(SocketAddr),
}

impl fmt::Display for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            #[cfg(unix)]
            Self::Unix(p) => write!(f, "unix:{}", p.display()),
            Self::Tcp(a) => write!(f, "tcp:{a}"),
        }
    }
}

#[async_trait]
impl Connector for Endpoint {
    async fn connect(&self) -> io::Result<BoxedStream> {
        match self {
            #[cfg(unix)]
            Self::Unix(p) => Ok(Box::new(tokio::net::UnixStream::connect(p).await?)),
            Self::Tcp(a) => {
                let s = tokio::net::TcpStream::connect(a).await?;
                s.set_nodelay(true)?;
                Ok(Box::new(s))
            }
        }
    }
}

type Pending = HashMap<i64, oneshot::Sender<RpcResponse>>;

#[derive(Default)]
struct State {
    pending: Pending,
    /// Why the connection is no longer usable; `Some` = closed.
    closed: Option<String>,
}

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn register(&self, id: i64, tx: oneshot::Sender<RpcResponse>) -> Result<()> {
        let mut st = self.lock();
        if let Some(reason) = &st.closed {
            return Err(SandboxError::Transport(format!(
                "crabd connection closed: {reason}"
            )));
        }
        st.pending.insert(id, tx);
        Ok(())
    }

    fn take(&self, id: i64) -> Option<oneshot::Sender<RpcResponse>> {
        self.lock().pending.remove(&id)
    }

    /// Mark closed (first reason wins) and fail every pending call.
    fn close(&self, reason: String) {
        let mut st = self.lock();
        if st.closed.is_none() {
            st.closed = Some(reason);
        }
        // Dropping the senders wakes every waiter with a "closed" error.
        st.pending.clear();
    }

    fn closed_reason(&self) -> Option<String> {
        self.lock().closed.clone()
    }
}

/// Removes a pending entry if the call future is dropped or times out.
struct PendingGuard<'a> {
    shared: &'a Shared,
    id: i64,
}

impl Drop for PendingGuard<'_> {
    fn drop(&mut self) {
        self.shared.take(self.id);
    }
}

/// Multiplexed client for one `crabd` connection.
pub struct CrabdClient {
    shared: Arc<Shared>,
    outbox: Mutex<Option<mpsc::Sender<Vec<u8>>>>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
    next_id: AtomicI64,
    default_timeout: Duration,
}

impl fmt::Debug for CrabdClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CrabdClient")
            .field("closed", &self.shared.closed_reason())
            .field("default_timeout", &self.default_timeout)
            .finish_non_exhaustive()
    }
}

impl CrabdClient {
    /// Start the client on `stream`. Must be called inside a tokio runtime.
    pub fn new<S>(stream: S) -> Self
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let (reader, writer) = tokio::io::split(stream);
        let shared = Arc::new(Shared::default());
        let (tx, rx) = mpsc::channel::<Vec<u8>>(64);
        let reader_task = tokio::spawn(reader_loop(reader, Arc::clone(&shared)));
        let writer_task = tokio::spawn(writer_loop(writer, rx, Arc::clone(&shared)));
        Self {
            shared,
            outbox: Mutex::new(Some(tx)),
            tasks: Mutex::new(vec![reader_task, writer_task]),
            next_id: AtomicI64::new(1),
            default_timeout: DEFAULT_CALL_TIMEOUT,
        }
    }

    /// Connect to any [`Connector`] (no handshake; see [`wait_ready`]).
    pub async fn connect(connector: &dyn Connector) -> Result<Self> {
        let stream = connector
            .connect()
            .await
            .map_err(|e| SandboxError::Transport(format!("connect {connector:?}: {e}")))?;
        Ok(Self::new(stream))
    }

    #[cfg(unix)]
    pub async fn connect_unix(path: impl Into<PathBuf>) -> Result<Self> {
        Self::connect(&Endpoint::Unix(path.into())).await
    }

    pub async fn connect_tcp(addr: SocketAddr) -> Result<Self> {
        Self::connect(&Endpoint::Tcp(addr)).await
    }

    /// Set the timeout used when a call does not imply a longer one.
    #[must_use]
    pub fn with_default_timeout(mut self, timeout: Duration) -> Self {
        self.default_timeout = timeout;
        self
    }

    #[must_use]
    pub fn default_timeout(&self) -> Duration {
        self.default_timeout
    }

    /// `Some(reason)` once the connection is unusable.
    #[must_use]
    pub fn closed_reason(&self) -> Option<String> {
        self.shared.closed_reason()
    }

    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.closed_reason().is_some()
    }

    /// Send any [`Call`] and wait for its raw `result` value.
    pub async fn request(&self, call: &Call, timeout: Duration) -> Result<Value> {
        let method = call.method();
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let frame = encode_frame(&call.to_request(id)).map_err(ProtocolError::from)?;
        let outbox = self
            .outbox
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
            .ok_or_else(|| SandboxError::Transport("crabd client is closed".into()))?;

        let (tx, rx) = oneshot::channel();
        self.shared.register(id, tx)?;
        let _guard = PendingGuard {
            shared: &self.shared,
            id,
        };

        let exchange = async {
            outbox.send(frame).await.map_err(|_| self.closed_error())?;
            rx.await.map_err(|_| self.closed_error())
        };
        let response = match tokio::time::timeout(timeout, exchange).await {
            Ok(r) => r?,
            Err(_) => {
                return Err(SandboxError::Timeout {
                    what: format!("crabd `{method}` call"),
                    after: timeout,
                });
            }
        };
        Ok(response.into_result()?)
    }

    /// `hello` handshake.
    pub async fn hello(&self) -> Result<HelloResult> {
        self.hello_with_timeout(self.default_timeout).await
    }

    pub async fn hello_with_timeout(&self, timeout: Duration) -> Result<HelloResult> {
        let call = Call::Hello(HelloParams {
            protocol_version: Some(PROTOCOL_VERSION.to_owned()),
            client: Some(concat!("sandcrab-core/", env!("CARGO_PKG_VERSION")).to_owned()),
        });
        let value = self.request(&call, timeout).await?;
        match Reply::from_method_value(METHOD_HELLO, value)? {
            Reply::Hello(h) => Ok(h),
            other => {
                Err(ProtocolError::Internal(format!("unexpected hello reply {other:?}")).into())
            }
        }
    }

    /// Invoke a tool with a timeout derived from the call ([`effective_timeout`]).
    pub async fn call(&self, call: ToolCall) -> Result<ToolResult> {
        let timeout = effective_timeout(&call, self.default_timeout);
        self.call_with_timeout(call, timeout).await
    }

    pub async fn call_with_timeout(&self, call: ToolCall, timeout: Duration) -> Result<ToolResult> {
        let method = call.name();
        let value = self.request(&Call::Tool(call), timeout).await?;
        Ok(ToolResult::from_method_value(method, value)?)
    }

    /// Close the connection: fail in-flight calls, flush and shut down the writer.
    pub async fn close(&self) {
        self.shared.close("client closed".into());
        let sender = self
            .outbox
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        drop(sender);
        let tasks: Vec<_> =
            std::mem::take(&mut *self.tasks.lock().unwrap_or_else(PoisonError::into_inner));
        for mut task in tasks {
            // The writer exits once the channel is closed; the reader may block on a
            // silent peer, so bound the wait and abort.
            if tokio::time::timeout(Duration::from_millis(200), &mut task)
                .await
                .is_err()
            {
                task.abort();
            }
        }
    }

    fn closed_error(&self) -> SandboxError {
        SandboxError::Transport(format!(
            "crabd connection closed: {}",
            self.closed_reason()
                .unwrap_or_else(|| "unknown reason".into())
        ))
    }
}

impl Drop for CrabdClient {
    fn drop(&mut self) {
        self.shared.close("client dropped".into());
        for task in self
            .tasks
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner)
            .drain(..)
        {
            task.abort();
        }
    }
}

/// Timeout for a tool call: `bash` with an explicit `timeout_ms` gets that plus
/// [`CALL_TIMEOUT_GRACE`] (if longer than the default); everything else the default.
#[must_use]
pub fn effective_timeout(call: &ToolCall, default: Duration) -> Duration {
    match call {
        ToolCall::Bash(p) => p
            .timeout_ms
            .map(|ms| Duration::from_millis(ms) + CALL_TIMEOUT_GRACE)
            .map_or(default, |t| t.max(default)),
        _ => default,
    }
}

async fn writer_loop<W: AsyncWrite + Unpin>(
    mut writer: W,
    mut rx: mpsc::Receiver<Vec<u8>>,
    shared: Arc<Shared>,
) {
    while let Some(frame) = rx.recv().await {
        let res = async {
            writer.write_all(&frame).await?;
            writer.flush().await
        }
        .await;
        if let Err(e) = res {
            shared.close(format!("write failed: {e}"));
            return;
        }
    }
    let _ = writer.shutdown().await;
}

async fn reader_loop<R: AsyncRead + Unpin>(mut reader: R, shared: Arc<Shared>) {
    let reason = loop {
        let mut header = [0u8; HEADER_LEN];
        if let Err(e) = reader.read_exact(&mut header).await {
            break if e.kind() == io::ErrorKind::UnexpectedEof {
                "connection closed by crabd".to_owned()
            } else {
                format!("read failed: {e}")
            };
        }
        let len = match parse_header(header) {
            Ok(len) => len,
            Err(e) => break format!("invalid frame header: {e}"),
        };
        let mut payload = vec![0u8; len];
        if let Err(e) = reader.read_exact(&mut payload).await {
            break format!("stream ended mid-frame: {e}");
        }
        match RpcMessage::from_slice(&payload) {
            Ok(RpcMessage::Response(resp)) => match &resp.id {
                Some(RequestId::Number(id)) => {
                    if let Some(tx) = shared.take(*id) {
                        let _ = tx.send(resp);
                    } else {
                        tracing::debug!(id, "dropping response for unknown or cancelled call");
                    }
                }
                _ => {
                    // crabd could not even read our request id: the stream is unusable.
                    break format!("crabd sent an unroutable response: {:?}", resp.payload);
                }
            },
            Ok(RpcMessage::Notification(n)) => {
                tracing::trace!(method = %n.method, "ignoring crabd notification");
            }
            Ok(RpcMessage::Request(r)) => {
                tracing::debug!(method = %r.method, "ignoring unexpected request from crabd");
            }
            Err(e) => break format!("malformed message from crabd: {e}"),
        }
    };
    shared.close(reason);
}

/// A connected, handshaken client.
#[derive(Debug)]
pub struct ReadyClient {
    pub client: CrabdClient,
    pub hello: HelloResult,
    /// Instant the first successful `hello` response was parsed (bench `spawn_ready` t1).
    pub ready_at: Instant,
    pub attempts: u32,
}

/// Backoff for [`wait_ready`]: start 2 ms, ×1.5, cap 20 ms (design §4).
const BACKOFF_START: Duration = Duration::from_millis(2);
const BACKOFF_CAP: Duration = Duration::from_millis(20);

/// Retry connect + `hello` until it succeeds or `deadline` passes.
///
/// A protocol version mismatch and an auth refusal (`PermissionDenied` from the connector)
/// are fatal (no retry).
pub async fn wait_ready(connector: &dyn Connector, deadline: Instant) -> Result<ReadyClient> {
    let started = Instant::now();
    let mut backoff = BACKOFF_START;
    let mut attempts = 0u32;
    let mut last_err = String::from("no attempt made");
    loop {
        let now = Instant::now();
        if now >= deadline {
            return Err(SandboxError::Timeout {
                what: format!(
                    "crabd readiness via {connector:?} after {attempts} attempt(s); last error: {last_err}"
                ),
                after: now - started,
            });
        }
        attempts += 1;
        let remaining = deadline - now;
        match tokio::time::timeout(remaining, connector.connect()).await {
            Ok(Ok(stream)) => {
                let client = CrabdClient::new(stream);
                match client
                    .hello_with_timeout(deadline.saturating_duration_since(Instant::now()))
                    .await
                {
                    Ok(hello) => {
                        let ready_at = Instant::now();
                        if hello.protocol_version != PROTOCOL_VERSION {
                            client.close().await;
                            return Err(ProtocolError::InvalidRequest(format!(
                                "crabd speaks protocol {}, host speaks {PROTOCOL_VERSION}",
                                hello.protocol_version
                            ))
                            .into());
                        }
                        return Ok(ReadyClient {
                            client,
                            hello,
                            ready_at,
                            attempts,
                        });
                    }
                    Err(e) => {
                        last_err = e.to_string();
                        client.close().await;
                    }
                }
            }
            // An explicit auth refusal will not fix itself by retrying.
            Ok(Err(e)) if e.kind() == io::ErrorKind::PermissionDenied => {
                return Err(SandboxError::Transport(format!(
                    "connect {connector:?}: {e}"
                )));
            }
            Ok(Err(e)) => last_err = format!("connect: {e}"),
            Err(_) => last_err = "connect timed out".into(),
        }
        let sleep = backoff.min(deadline.saturating_duration_since(Instant::now()));
        tokio::time::sleep(sleep).await;
        backoff = (backoff.mul_f64(1.5)).min(BACKOFF_CAP);
    }
}

/// Open a `tunnel.open` byte stream to guest loopback `port` on a **fresh** connection from
/// `connector`.
///
/// Sends the `tunnel.open` frame, reads exactly the ack frame (never past it), and returns
/// the stream, which from then on carries raw bytes to and from `127.0.0.1:<port>` inside
/// the guest. A refusal from `crabd` (nothing listening, tunnel limit, bad port) comes back
/// as the typed protocol error; `timeout` bounds connect + handshake.
pub async fn open_tunnel(
    connector: &dyn Connector,
    port: u16,
    timeout: Duration,
) -> Result<BoxedStream> {
    let params = TunnelOpenParams::new(port)?;
    let handshake = async {
        let mut stream = connector
            .connect()
            .await
            .map_err(|e| SandboxError::Transport(format!("connect {connector:?}: {e}")))?;
        let frame = encode_frame(&params.to_request(1)).map_err(ProtocolError::from)?;
        stream
            .write_all(&frame)
            .await
            .map_err(|e| SandboxError::Transport(format!("send tunnel.open: {e}")))?;
        stream
            .flush()
            .await
            .map_err(|e| SandboxError::Transport(format!("send tunnel.open: {e}")))?;
        let resp = read_one_response(&mut stream).await?;
        let ack = TunnelOpenAck::from_value(resp.into_result()?)?;
        if ack.port != port {
            return Err(SandboxError::Transport(format!(
                "crabd acked a tunnel to port {} instead of {port}",
                ack.port
            )));
        }
        Ok(stream)
    };
    tokio::time::timeout(timeout, handshake)
        .await
        .map_err(|_| SandboxError::Timeout {
            what: format!("tunnel.open to guest port {port}"),
            after: timeout,
        })?
}

/// Read exactly one response frame (header + payload, nothing more).
pub(crate) async fn read_one_response<R: AsyncRead + Unpin + ?Sized>(
    reader: &mut R,
) -> Result<RpcResponse> {
    let mut header = [0u8; HEADER_LEN];
    reader.read_exact(&mut header).await.map_err(|e| {
        SandboxError::Transport(if e.kind() == io::ErrorKind::UnexpectedEof {
            "crabd closed the connection before answering".to_owned()
        } else {
            format!("read failed: {e}")
        })
    })?;
    let len = parse_header(header).map_err(ProtocolError::from)?;
    let mut payload = vec![0u8; len];
    reader
        .read_exact(&mut payload)
        .await
        .map_err(|e| SandboxError::Transport(format!("stream ended mid-frame: {e}")))?;
    match RpcMessage::from_slice(&payload)? {
        RpcMessage::Response(r) => Ok(r),
        other => Err(SandboxError::Transport(format!(
            "expected a response from crabd, got {other:?}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicU32;

    use sandcrab_protocol::{BashParams, BashResult, FileReadParams};

    use super::*;
    use crate::testing::{MockConnector, MockCrabd, bash};

    fn duplex_client(mock: MockCrabd) -> CrabdClient {
        let (a, b) = tokio::io::duplex(1 << 16);
        tokio::spawn(mock.serve(b));
        CrabdClient::new(a)
    }

    #[tokio::test]
    async fn hello_and_call() {
        let c = duplex_client(MockCrabd::default());
        let h = c.hello().await.unwrap();
        assert_eq!(h.protocol_version, PROTOCOL_VERSION);
        let r: BashResult = c.call(bash("echo hi")).await.unwrap().try_into().unwrap();
        assert_eq!(r.stdout, "echo hi");
        assert_eq!(r.exit_code, Some(0));
    }

    #[tokio::test]
    async fn rpc_errors_are_typed() {
        let c = duplex_client(MockCrabd::default());
        let err = c
            .call(ToolCall::FileRead(FileReadParams {
                path: "/missing".into(),
                offset: None,
                limit: None,
            }))
            .await
            .unwrap_err();
        assert!(
            matches!(err, SandboxError::Protocol(ProtocolError::NotFound(_))),
            "{err:?}"
        );
        // Connection stays usable after an error reply.
        c.call(bash("ok")).await.unwrap();
    }

    #[tokio::test]
    async fn concurrent_calls_complete_out_of_order() {
        let c = Arc::new(duplex_client(MockCrabd::default()));
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut tasks = Vec::new();
        for (i, delay) in [400u64, 100, 200, 0, 300].into_iter().enumerate() {
            let c = Arc::clone(&c);
            let order = Arc::clone(&order);
            tasks.push(tokio::spawn(async move {
                let r: BashResult = c
                    .call(bash(&format!("sleep:{delay}:{i}")))
                    .await
                    .unwrap()
                    .try_into()
                    .unwrap();
                assert_eq!(r.stdout, format!("sleep:{delay}:{i}"));
                order.lock().unwrap().push(i);
            }));
        }
        for t in tasks {
            t.await.unwrap();
        }
        // Completion order follows the delays, not the send order.
        assert_eq!(*order.lock().unwrap(), vec![3, 1, 2, 4, 0]);
    }

    #[tokio::test]
    async fn call_timeout_does_not_poison_connection() {
        let c = duplex_client(MockCrabd::default());
        let err = c
            .call_with_timeout(bash("sleep:500:slow"), Duration::from_millis(30))
            .await
            .unwrap_err();
        assert!(matches!(err, SandboxError::Timeout { .. }), "{err:?}");
        // The late response is discarded; later calls still work.
        let r: BashResult = c.call(bash("after")).await.unwrap().try_into().unwrap();
        assert_eq!(r.stdout, "after");
        assert!(c.shared.lock().pending.is_empty());
    }

    #[tokio::test]
    async fn cancelled_call_cleans_up() {
        let c = duplex_client(MockCrabd::default());
        let fut = c.call(bash("sleep:300:x"));
        let _ = tokio::time::timeout(Duration::from_millis(20), fut).await;
        assert!(c.shared.lock().pending.is_empty());
        c.call(bash("still ok")).await.unwrap();
    }

    #[tokio::test]
    async fn server_hangup_fails_pending_and_future_calls() {
        let c = duplex_client(MockCrabd::default());
        let err = c.call(bash("hangup")).await.unwrap_err();
        assert!(matches!(err, SandboxError::Transport(_)), "{err:?}");
        assert!(c.is_closed());
        assert!(matches!(
            c.call(bash("x")).await,
            Err(SandboxError::Transport(_))
        ));
    }

    #[tokio::test]
    async fn garbage_frame_closes_connection() {
        let (a, mut b) = tokio::io::duplex(1024);
        let c = CrabdClient::new(a);
        let pending = tokio::spawn(async move { c.call(bash("x")).await.map(|_| ()) });
        // Read the request, answer with an oversized header.
        let mut hdr = [0u8; HEADER_LEN];
        b.read_exact(&mut hdr).await.unwrap();
        let mut body = vec![0; parse_header(hdr).unwrap()];
        b.read_exact(&mut body).await.unwrap();
        b.write_all(&u32::MAX.to_be_bytes()).await.unwrap();
        let err = pending.await.unwrap().unwrap_err();
        assert!(
            matches!(err, SandboxError::Transport(ref m) if m.contains("header")),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn close_is_clean() {
        let c = duplex_client(MockCrabd::default());
        c.hello().await.unwrap();
        c.close().await;
        assert!(c.is_closed());
        assert!(matches!(c.hello().await, Err(SandboxError::Transport(_))));
    }

    #[test]
    fn timeouts_follow_bash_timeout() {
        let d = DEFAULT_CALL_TIMEOUT;
        assert_eq!(effective_timeout(&bash("x"), d), d);
        let long = ToolCall::Bash(BashParams {
            command: "x".into(),
            cwd: None,
            env: Default::default(),
            timeout_ms: Some(600_000),
        });
        assert_eq!(
            effective_timeout(&long, d),
            Duration::from_secs(600) + CALL_TIMEOUT_GRACE
        );
        let short = ToolCall::Bash(BashParams {
            timeout_ms: Some(10),
            ..match bash("x") {
                ToolCall::Bash(p) => p,
                _ => unreachable!(),
            }
        });
        assert_eq!(effective_timeout(&short, d), d);
    }

    #[tokio::test]
    async fn unix_socket_transport() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("crabd.sock");
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        tokio::spawn(async move {
            while let Ok((s, _)) = listener.accept().await {
                tokio::spawn(MockCrabd::default().serve(s));
            }
        });
        let c = CrabdClient::connect_unix(&sock).await.unwrap();
        c.hello().await.unwrap();
        c.call(bash("over uds")).await.unwrap();
    }

    #[tokio::test]
    async fn tcp_transport() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((s, _)) = listener.accept().await {
                tokio::spawn(MockCrabd::default().serve(s));
            }
        });
        let c = CrabdClient::connect_tcp(addr).await.unwrap();
        c.call(bash("over tcp")).await.unwrap();
    }

    #[tokio::test]
    async fn wait_ready_retries_until_up() {
        let fails = Arc::new(AtomicU32::new(3));
        let conn = MockConnector::new(MockCrabd::default()).failing_first(Arc::clone(&fails));
        let ready = wait_ready(&conn, Instant::now() + Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(ready.attempts, 4);
        assert_eq!(ready.hello.default_cwd, "/workspace");
        ready.client.call(bash("x")).await.unwrap();
    }

    #[tokio::test]
    async fn wait_ready_retries_failed_hello() {
        let conn = MockConnector::new(MockCrabd {
            hello_failures: Arc::new(AtomicU32::new(2)),
            ..MockCrabd::default()
        });
        let ready = wait_ready(&conn, Instant::now() + Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(ready.attempts, 3);
    }

    #[tokio::test]
    async fn wait_ready_times_out() {
        let conn = MockConnector::new(MockCrabd::default())
            .failing_first(Arc::new(AtomicU32::new(u32::MAX)));
        let start = Instant::now();
        let err = wait_ready(&conn, start + Duration::from_millis(100))
            .await
            .unwrap_err();
        assert!(matches!(err, SandboxError::Timeout { .. }), "{err:?}");
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[tokio::test]
    async fn wait_ready_rejects_protocol_mismatch() {
        let conn = MockConnector::new(MockCrabd {
            protocol_version: "99.0".into(),
            ..MockCrabd::default()
        });
        let err = wait_ready(&conn, Instant::now() + Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(matches!(err, SandboxError::Protocol(_)), "{err:?}");
    }

    /// Connector whose single stream is served by a scripted fake `crabd`.
    #[derive(Debug)]
    struct ScriptedTunnel {
        ack_port: u16,
        trailing: &'static [u8],
    }

    #[async_trait]
    impl Connector for ScriptedTunnel {
        async fn connect(&self) -> io::Result<BoxedStream> {
            let (a, mut b) = tokio::io::duplex(1 << 16);
            let (ack_port, trailing) = (self.ack_port, self.trailing);
            tokio::spawn(async move {
                let mut header = [0u8; HEADER_LEN];
                b.read_exact(&mut header).await.unwrap();
                let mut req = vec![0u8; parse_header(header).unwrap()];
                b.read_exact(&mut req).await.unwrap();
                let RpcMessage::Request(req) = RpcMessage::from_slice(&req).unwrap() else {
                    panic!("request")
                };
                assert_eq!(req.method, sandcrab_protocol::METHOD_TUNNEL_OPEN);
                let ack = RpcResponse::success(req.id, serde_json::json!({ "port": ack_port }));
                // Ack and the first raw bytes in one write: the client must not swallow them.
                let mut out = encode_frame(&ack).unwrap();
                out.extend_from_slice(trailing);
                b.write_all(&out).await.unwrap();
                let mut sink = Vec::new();
                let _ = b.read_to_end(&mut sink).await;
            });
            Ok(Box::new(a))
        }
    }

    #[tokio::test]
    async fn open_tunnel_does_not_read_past_the_ack() {
        let conn = ScriptedTunnel {
            ack_port: 8080,
            trailing: b"HTTP/1.1 200 OK\r\n",
        };
        let mut s = open_tunnel(&conn, 8080, Duration::from_secs(5))
            .await
            .unwrap();
        let mut got = vec![0u8; conn.trailing.len()];
        s.read_exact(&mut got).await.unwrap();
        assert_eq!(got, conn.trailing);
    }

    #[tokio::test]
    async fn open_tunnel_rejects_wrong_port_ack_and_port_zero() {
        let conn = ScriptedTunnel {
            ack_port: 9,
            trailing: b"",
        };
        let err = open_tunnel(&conn, 8080, Duration::from_secs(5))
            .await
            .err()
            .unwrap();
        assert!(matches!(err, SandboxError::Transport(_)), "{err:?}");
        let err = open_tunnel(&conn, 0, Duration::from_secs(5))
            .await
            .err()
            .unwrap();
        assert!(matches!(err, SandboxError::Protocol(_)), "{err:?}");
    }
}
