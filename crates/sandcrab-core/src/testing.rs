//! Test doubles: an in-process mock `crabd` speaking the real framing.
//!
//! Compiled for this crate's tests and, with the `test-util` feature, for downstream crates.

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use sandcrab_protocol::{
    BashParams, BashResult, Call, HEADER_LEN, HelloResult, PROTOCOL_VERSION, PingResult,
    ProtocolError, Reply, RpcMessage, RpcResponse, TOOL_NAMES, ToolCall, ToolResult, encode_frame,
    parse_header,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::client::{BoxedStream, Connector};

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
#[derive(Debug, Clone)]
pub struct MockCrabd {
    pub protocol_version: String,
    pub default_cwd: String,
    /// The next N `hello` calls fail with an internal error.
    pub hello_failures: Arc<AtomicU32>,
    /// Tool calls served so far.
    pub tool_calls: Arc<AtomicU32>,
}

impl Default for MockCrabd {
    fn default() -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION.to_owned(),
            default_cwd: "/workspace".to_owned(),
            hello_failures: Arc::new(AtomicU32::new(0)),
            tool_calls: Arc::new(AtomicU32::new(0)),
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
                    })),
                )
            }
            Call::Ping => (Duration::ZERO, Ok(Reply::Ping(PingResult {}))),
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
