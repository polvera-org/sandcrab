//! Test doubles: an in-process mock `crabd` speaking the real framing, and a fake
//! [`RuntimeBackend`] whose sandboxes are served by it.
//!
//! Compiled for this crate's tests and, with the `test-util` feature, for downstream crates.

use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use sandcrab_protocol::{
    BashParams, BashResult, Call, HEADER_LEN, HelloResult, METHOD_TUNNEL_OPEN, PROTOCOL_VERSION,
    PingResult, ProtocolError, Reply, RpcMessage, RpcRequest, RpcResponse, TOOL_NAMES, ToolCall,
    ToolResult, TunnelOpenAck, TunnelOpenParams, encode_frame, parse_header,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::client::{BoxedStream, Connector};
use crate::{
    BackendCapabilities, BackendKind, LifecycleTimings, NetworkConfig, NetworkPolicy,
    PublishedPort, ResolvedSpec, Result, RuntimeBackend, SandboxError, SandboxHandle, SandboxId,
};

/// `bash` call with defaults.
#[must_use]
pub fn bash(command: &str) -> ToolCall {
    ToolCall::Bash(BashParams {
        command: command.into(),
        cwd: None,
        env: Default::default(),
        timeout_ms: None,
    })
}

/// Decrement `n` if positive; true if it was positive.
fn consume(n: &AtomicU32) -> bool {
    let mut cur = n.load(Ordering::SeqCst);
    while cur > 0 {
        match n.compare_exchange(cur, cur - 1, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => return true,
            Err(actual) => cur = actual,
        }
    }
    false
}

/// Mock `crabd`. Behaviour of `bash` by command:
/// - `sleep:<ms>:<tag>` replies after `<ms>` ms (requests are served concurrently);
/// - `hangup` closes the connection without replying;
/// - anything else replies immediately with `stdout = command`, exit 0.
///
/// `file_read` always fails with `NotFound`; other tools reply `Unimplemented`.
///
/// `tunnel.open` as a connection's first frame dials `127.0.0.1:<port>` **on the test host**
/// (standing in for guest loopback) and relays raw bytes like the real `crabd`; nothing
/// listening → `-32004`.
#[derive(Debug, Clone)]
pub struct MockCrabd {
    pub protocol_version: String,
    pub default_cwd: String,
    /// The next N `hello` calls fail with an internal error.
    pub hello_failures: Arc<AtomicU32>,
    /// Tool calls served so far.
    pub tool_calls: Arc<AtomicU32>,
    /// Tunnels opened (acked) so far.
    pub tunnels: Arc<AtomicU32>,
}

impl Default for MockCrabd {
    fn default() -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION.to_owned(),
            default_cwd: "/workspace".to_owned(),
            hello_failures: Arc::new(AtomicU32::new(0)),
            tool_calls: Arc::new(AtomicU32::new(0)),
            tunnels: Arc::new(AtomicU32::new(0)),
        }
    }
}

impl MockCrabd {
    /// Serve one connection until EOF.
    pub async fn serve<S>(self, stream: S)
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let (mut reader, writer) = tokio::io::split(stream);
        let writer = Arc::new(tokio::sync::Mutex::new(writer));
        let mut first = true;
        loop {
            let mut header = [0u8; HEADER_LEN];
            if reader.read_exact(&mut header).await.is_err() {
                return;
            }
            let Ok(len) = parse_header(header) else {
                return;
            };
            let mut payload = vec![0u8; len];
            if reader.read_exact(&mut payload).await.is_err() {
                return;
            }
            let Ok(RpcMessage::Request(req)) = RpcMessage::from_slice(&payload) else {
                return;
            };
            if std::mem::take(&mut first) && req.method == METHOD_TUNNEL_OPEN {
                // Nothing else holds the writer before the first request is dispatched.
                let Ok(writer) = Arc::try_unwrap(writer) else {
                    return;
                };
                self.serve_tunnel(reader.unsplit(writer.into_inner()), req)
                    .await;
                return;
            }
            let call = Call::from_request(&req);
            if matches!(&call, Ok(Call::Tool(ToolCall::Bash(p))) if p.command == "hangup") {
                let _ = writer.lock().await.shutdown().await;
                return;
            }
            let me = self.clone();
            let writer = Arc::clone(&writer);
            tokio::spawn(async move {
                let (delay, outcome) = me.respond(call);
                if !delay.is_zero() {
                    tokio::time::sleep(delay).await;
                }
                let resp = RpcResponse::from_outcome(req.id, outcome);
                let frame = encode_frame(&resp).expect("encode");
                let mut w = writer.lock().await;
                let _ = w.write_all(&frame).await;
                let _ = w.flush().await;
            });
        }
    }

    /// `tunnel.open` as the first frame: dial **this host's** `127.0.0.1:<port>` (the mock's
    /// "guest loopback"), ack, then relay raw bytes with half-close until both sides are done.
    /// Nothing listening → `-32004`, like the real `crabd`.
    async fn serve_tunnel<S>(&self, mut stream: S, req: RpcRequest)
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        async fn reply<S: AsyncWrite + Unpin>(stream: &mut S, resp: RpcResponse) {
            let frame = encode_frame(&resp).expect("encode");
            let _ = stream.write_all(&frame).await;
            let _ = stream.flush().await;
        }
        let params = match TunnelOpenParams::from_request(&req) {
            Ok(p) => p,
            Err(e) => {
                let resp = RpcResponse::failure(Some(req.id), &e);
                return reply(&mut stream, resp).await;
            }
        };
        let mut target =
            match tokio::net::TcpStream::connect((Ipv4Addr::LOCALHOST, params.port)).await {
                Ok(t) => t,
                Err(e) => {
                    let e = if e.kind() == io::ErrorKind::ConnectionRefused {
                        ProtocolError::NotFound(format!(
                            "nothing is listening on guest port {}",
                            params.port
                        ))
                    } else {
                        ProtocolError::ExecutionFailed(e.to_string())
                    };
                    let resp = RpcResponse::failure(Some(req.id), &e);
                    return reply(&mut stream, resp).await;
                }
            };
        let ack = RpcResponse::success(req.id, TunnelOpenAck { port: params.port }.to_value());
        reply(&mut stream, ack).await;
        self.tunnels.fetch_add(1, Ordering::SeqCst);
        let _ = tokio::io::copy_bidirectional(&mut stream, &mut target).await;
    }

    fn respond(
        &self,
        call: Result<Call, ProtocolError>,
    ) -> (Duration, Result<Reply, ProtocolError>) {
        let call = match call {
            Ok(c) => c,
            Err(e) => return (Duration::ZERO, Err(e)),
        };
        match call {
            Call::Hello(_) => {
                if consume(&self.hello_failures) {
                    return (
                        Duration::ZERO,
                        Err(ProtocolError::Internal("not ready yet".into())),
                    );
                }
                (
                    Duration::ZERO,
                    Ok(Reply::Hello(HelloResult {
                        crabd_version: "mock".into(),
                        protocol_version: self.protocol_version.clone(),
                        tools: TOOL_NAMES.iter().map(|s| (*s).to_owned()).collect(),
                        default_cwd: self.default_cwd.clone(),
                        features: Vec::new(),
                        forward_ports: Vec::new(),
                    })),
                )
            }
            Call::Ping => (Duration::ZERO, Ok(Reply::Ping(PingResult {}))),
            other @ (Call::ProcessCancel(_) | Call::FsIdentity(_) | Call::ForwardStats) => (
                Duration::ZERO,
                Err(ProtocolError::MethodNotFound(other.method().to_owned())),
            ),
            Call::Tool(tool) => {
                self.tool_calls.fetch_add(1, Ordering::SeqCst);
                match tool {
                    ToolCall::Bash(p) => {
                        let delay = p
                            .command
                            .strip_prefix("sleep:")
                            .and_then(|rest| rest.split(':').next())
                            .and_then(|ms| ms.parse().ok())
                            .map_or(Duration::ZERO, Duration::from_millis);
                        (
                            delay,
                            Ok(Reply::Tool(ToolResult::Bash(BashResult {
                                exit_code: Some(0),
                                signal: None,
                                stdout: p.command,
                                stderr: String::new(),
                                truncated: false,
                                timed_out: false,
                                duration_ms: 0,
                            }))),
                        )
                    }
                    ToolCall::FileRead(p) => (Duration::ZERO, Err(ProtocolError::NotFound(p.path))),
                    other => (
                        Duration::ZERO,
                        Err(ProtocolError::Unimplemented(other.name().into())),
                    ),
                }
            }
        }
    }
}

/// [`Connector`] that serves each connection with a [`MockCrabd`] over a tokio duplex.
#[derive(Debug, Clone)]
pub struct MockConnector {
    pub mock: MockCrabd,
    /// The next N connects fail with `ConnectionRefused`.
    pub fail_first: Arc<AtomicU32>,
    pub connects: Arc<AtomicU32>,
}

impl MockConnector {
    #[must_use]
    pub fn new(mock: MockCrabd) -> Self {
        Self {
            mock,
            fail_first: Arc::new(AtomicU32::new(0)),
            connects: Arc::new(AtomicU32::new(0)),
        }
    }

    #[must_use]
    pub fn failing_first(mut self, n: Arc<AtomicU32>) -> Self {
        self.fail_first = n;
        self
    }
}

#[async_trait]
impl Connector for MockConnector {
    async fn connect(&self) -> io::Result<BoxedStream> {
        self.connects.fetch_add(1, Ordering::SeqCst);
        if consume(&self.fail_first) {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionRefused,
                "mock crabd not up yet",
            ));
        }
        let (a, b) = tokio::io::duplex(1 << 16);
        tokio::spawn(self.mock.clone().serve(b));
        Ok(Box::new(a))
    }
}

/// What a [`FakeBackend`] observed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FakeEvent {
    Spawned(SandboxId),
    Destroyed(SandboxId),
}

/// In-memory [`RuntimeBackend`]: every sandbox is a [`MockCrabd`] reached over a duplex.
#[derive(Debug, Clone)]
pub struct FakeBackend {
    pub kind: BackendKind,
    pub caps: BackendCapabilities,
    /// Network policies this backend claims to enforce.
    pub enforced: Vec<NetworkPolicy>,
    pub mock: MockCrabd,
    /// Connect attempts to fail (shared by all sandboxes) — e.g. `u32::MAX` = never ready.
    pub connect_failures: Arc<AtomicU32>,
    /// Make `destroy` fail.
    pub fail_destroy: Arc<AtomicBool>,
    /// Milliseconds each `destroy` sleeps before completing (slow teardown).
    pub destroy_delay_ms: Arc<AtomicU64>,
    events: Arc<Mutex<Vec<FakeEvent>>>,
    last_spec: Arc<Mutex<Option<ResolvedSpec>>>,
}

impl FakeBackend {
    /// Hardware-isolated (Firecracker-like) fake that enforces only `disabled`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            kind: BackendKind::Firecracker,
            caps: BackendCapabilities {
                hardware_isolated: true,
                multi_tenant_safe: false,
                supports_snapshots: false,
                supports_host_dir_mounts: false,
            },
            enforced: vec![NetworkPolicy::Disabled],
            mock: MockCrabd::default(),
            connect_failures: Arc::new(AtomicU32::new(0)),
            fail_destroy: Arc::new(AtomicBool::new(false)),
            destroy_delay_ms: Arc::new(AtomicU64::new(0)),
            events: Arc::default(),
            last_spec: Arc::default(),
        }
    }

    /// Shared-kernel (Docker-like) fake enforcing `disabled` and `unrestricted`.
    #[must_use]
    pub fn shared_kernel() -> Self {
        Self {
            kind: BackendKind::Docker,
            caps: BackendCapabilities {
                hardware_isolated: false,
                multi_tenant_safe: false,
                supports_snapshots: false,
                supports_host_dir_mounts: true,
            },
            enforced: vec![NetworkPolicy::Disabled, NetworkPolicy::Unrestricted],
            ..Self::new()
        }
    }

    #[must_use]
    pub fn events(&self) -> Vec<FakeEvent> {
        self.events
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    #[must_use]
    pub fn last_spec(&self) -> Option<ResolvedSpec> {
        self.last_spec
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn push(&self, e: FakeEvent) {
        self.events
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(e);
    }
}

impl Default for FakeBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl RuntimeBackend for FakeBackend {
    fn kind(&self) -> BackendKind {
        self.kind
    }

    fn capabilities(&self) -> BackendCapabilities {
        self.caps
    }

    fn enforces_network_policy(&self, network: &NetworkConfig) -> bool {
        self.enforced.contains(&network.policy)
    }

    async fn spawn(&self, spec: &ResolvedSpec) -> Result<Box<dyn SandboxHandle>> {
        spec.reject_secrets()?;
        if !self.enforces_network_policy(&spec.network) {
            return Err(SandboxError::PolicyUnenforceable(
                spec.network.policy.to_string(),
            ));
        }
        *self
            .last_spec
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(spec.clone());
        self.push(FakeEvent::Spawned(spec.id));
        spec.timings.mark_custom("fake_started");
        let ports = spec
            .ports
            .iter()
            .map(|p| PublishedPort {
                guest: p.guest,
                host: SocketAddr::from((Ipv4Addr::LOCALHOST, 40_000 + p.guest % 20_000)),
            })
            .collect();
        Ok(Box::new(FakeHandle {
            id: spec.id,
            connector: Arc::new(
                MockConnector::new(self.mock.clone())
                    .failing_first(Arc::clone(&self.connect_failures)),
            ),
            ports,
            timings: spec.timings.clone(),
            backend: self.clone(),
        }))
    }
}

struct FakeHandle {
    id: SandboxId,
    connector: Arc<MockConnector>,
    ports: Vec<PublishedPort>,
    timings: LifecycleTimings,
    backend: FakeBackend,
}

#[async_trait]
impl SandboxHandle for FakeHandle {
    fn id(&self) -> SandboxId {
        self.id
    }

    fn connector(&self) -> Arc<dyn Connector> {
        self.connector.clone()
    }

    fn published_ports(&self) -> Vec<PublishedPort> {
        self.ports.clone()
    }

    fn timings(&self) -> LifecycleTimings {
        self.timings.clone()
    }

    async fn destroy(self: Box<Self>) -> Result<()> {
        let delay = self.backend.destroy_delay_ms.load(Ordering::SeqCst);
        if delay > 0 {
            tokio::time::sleep(Duration::from_millis(delay)).await;
        }
        if self.backend.fail_destroy.load(Ordering::SeqCst) {
            return Err(SandboxError::Backend {
                backend: self.backend.kind.to_string(),
                message: "injected destroy failure".into(),
            });
        }
        self.backend.push(FakeEvent::Destroyed(self.id));
        Ok(())
    }
}
