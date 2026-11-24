//! Host client of the guest `init.*` control channel over `ctl.sock` (design §6, §7).
//!
//! libkrun maps the guest's vsock control port ([`CONTROL_PORT`](crate::helper_proto::CONTROL_PORT))
//! to a host Unix socket in the run dir. There is no `CONNECT` handshake (unlike Firecracker):
//! a plain `UnixStream` reaches `crabd init`. The core [`sandcrab_core::CrabdClient`] only
//! speaks `hello` / tools, so this is a small sequential JSON-RPC client using the shared
//! protocol framing and the `sandcrab_protocol::init` types (the same handshake as the
//! Firecracker backend).
//!
//! Before the guest listens, libkrun may accept the host connection and close it at once, so
//! a successful `connect()` is not readiness: [`connect_control`] probes each connection with
//! `ping` and retries (backoff 2 ms ×1.5, capped at 20 ms) until it answers.

use std::path::Path;
use std::time::{Duration, Instant};

use sandcrab_core::{Result, SandboxError};
use sandcrab_protocol::init::{
    InitConfig, InitConfigureResult, InitShutdownParams, InitShutdownResult, METHOD_CONFIGURE,
    METHOD_SHUTDOWN,
};
use sandcrab_protocol::{
    HEADER_LEN, METHOD_PING, ProtocolError, RpcMessage, RpcRequest, encode_frame, parse_header,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::UnixStream;

/// How long one readiness probe (`ping`) may take before the connection is retried.
const PROBE_TIMEOUT: Duration = Duration::from_millis(250);
const BACKOFF_START: Duration = Duration::from_millis(2);
const BACKOFF_CAP: Duration = Duration::from_millis(20);

/// One control connection (requests are sequential).
#[derive(Debug)]
pub struct ControlClient<S> {
    stream: S,
    next_id: i64,
}

impl<S: AsyncRead + AsyncWrite + Unpin + Send> ControlClient<S> {
    pub fn new(stream: S) -> Self {
        Self { stream, next_id: 1 }
    }

    /// Send `method` and wait (up to `timeout`) for its result.
    pub async fn call<P: Serialize, R: DeserializeOwned>(
        &mut self,
        method: &str,
        params: &P,
        timeout: Duration,
    ) -> Result<R> {
        let exchange = async {
            let id = self.next_id;
            self.next_id += 1;
            let params = serde_json::to_value(params)
                .map_err(|e| ProtocolError::Internal(format!("encode {method}: {e}")))?;
            let frame = encode_frame(&RpcRequest::new(id, method, Some(params)))
                .map_err(ProtocolError::from)?;
            self.stream.write_all(&frame).await.map_err(transport)?;
            self.stream.flush().await.map_err(transport)?;
            loop {
                let payload = read_frame(&mut self.stream).await?;
                match RpcMessage::from_slice(&payload)? {
                    RpcMessage::Response(resp) => {
                        let value = resp.into_result()?;
                        return serde_json::from_value::<R>(value).map_err(|e| {
                            SandboxError::from(ProtocolError::Internal(format!(
                                "bad {method} result: {e}"
                            )))
                        });
                    }
                    // Notifications / requests are not part of the control contract.
                    _ => continue,
                }
            }
        };
        tokio::time::timeout(timeout, exchange)
            .await
            .map_err(|_| SandboxError::Timeout {
                what: format!("guest control `{method}`"),
                after: timeout,
            })?
    }

    pub async fn ping(&mut self, timeout: Duration) -> Result<()> {
        self.call::<_, serde_json::Value>(METHOD_PING, &serde_json::json!({}), timeout)
            .await
            .map(drop)
    }

    pub async fn configure(
        &mut self,
        cfg: &InitConfig,
        timeout: Duration,
    ) -> Result<InitConfigureResult> {
        self.call(METHOD_CONFIGURE, cfg, timeout).await
    }

    /// Request shutdown; the guest acks, then syncs, unmounts and reboots (the VMM exits).
    pub async fn shutdown(&mut self, grace_ms: u64, timeout: Duration) -> Result<()> {
        self.call::<_, InitShutdownResult>(
            METHOD_SHUTDOWN,
            &InitShutdownParams { grace_ms },
            timeout,
        )
        .await
        .map(drop)
    }
}

fn transport(e: std::io::Error) -> SandboxError {
    SandboxError::Transport(format!("guest control channel: {e}"))
}

async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> Result<Vec<u8>> {
    let mut header = [0u8; HEADER_LEN];
    r.read_exact(&mut header).await.map_err(transport)?;
    let len = parse_header(header).map_err(ProtocolError::from)?;
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload).await.map_err(transport)?;
    Ok(payload)
}

/// Connect to `ctl.sock` and wait until `crabd init` answers `ping`, retrying until
/// `deadline`. Returns the ready client and the number of attempts.
pub async fn connect_control(
    sock: &Path,
    deadline: Instant,
) -> Result<(ControlClient<UnixStream>, u32)> {
    let started = Instant::now();
    let mut backoff = BACKOFF_START;
    let mut attempts = 0u32;
    loop {
        attempts += 1;
        let remaining = deadline.saturating_duration_since(Instant::now());
        let err = match tokio::time::timeout(remaining, UnixStream::connect(sock)).await {
            Ok(Ok(s)) => {
                let mut c = ControlClient::new(s);
                match c.ping(PROBE_TIMEOUT.min(remaining)).await {
                    Ok(()) => return Ok((c, attempts)),
                    Err(e) => format!("probe: {e}"),
                }
            }
            Ok(Err(e)) => format!("connect: {e}"),
            Err(_) => "connect timed out".to_owned(),
        };
        if Instant::now() >= deadline {
            return Err(SandboxError::Timeout {
                what: format!(
                    "guest control socket {} after {attempts} attempt(s); last error: {err}",
                    sock.display()
                ),
                after: started.elapsed(),
            });
        }
        tokio::time::sleep(backoff.min(deadline.saturating_duration_since(Instant::now()))).await;
        backoff = backoff.mul_f64(1.5).min(BACKOFF_CAP);
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use sandcrab_protocol::RpcResponse;
    use sandcrab_protocol::init::{GuestMount, InitUser};
    use tokio::net::UnixListener;

    use super::*;

    pub(crate) fn init_cfg() -> InitConfig {
        InitConfig {
            sandbox_id: "sb".into(),
            hostname: "sc-1".into(),
            user: InitUser {
                uid: 501,
                gid: 20,
                home: None,
                name: None,
            },
            env: Default::default(),
            default_cwd: "/workspace".into(),
            mounts: vec![GuestMount::virtio_fs("ws0", "/workspace", false)],
            network: false,
            dns: vec![],
            tools_port: 1024,
            host_time_unix_ms: Some(1),
        }
    }

    /// Minimal `crabd init` control server: ping, configure, shutdown.
    pub(crate) async fn fake_init<S: AsyncRead + AsyncWrite + Unpin>(mut s: S) {
        loop {
            let Ok(payload) = read_frame(&mut s).await else {
                return;
            };
            let RpcMessage::Request(req) = RpcMessage::from_slice(&payload).unwrap() else {
                continue;
            };
            let reply = match req.method.as_str() {
                METHOD_PING | METHOD_SHUTDOWN => {
                    RpcResponse::success(req.id, serde_json::json!({}))
                }
                METHOD_CONFIGURE => {
                    let c: InitConfig = serde_json::from_value(req.params.unwrap()).unwrap();
                    assert_eq!(
                        c.mounts[0],
                        GuestMount::virtio_fs("ws0", "/workspace", false)
                    );
                    RpcResponse::success(req.id, serde_json::json!({"serve_pid": 9, "boot_ms": 80}))
                }
                _ => RpcResponse::failure(
                    Some(req.id),
                    &ProtocolError::MethodNotFound(req.method.clone()),
                ),
            };
            s.write_all(&encode_frame(&reply).unwrap()).await.unwrap();
        }
    }

    #[tokio::test]
    async fn configure_ping_shutdown() {
        let (a, b) = tokio::io::duplex(4096);
        tokio::spawn(fake_init(b));
        let mut c = ControlClient::new(a);
        let t = Duration::from_secs(1);
        let r = c.configure(&init_cfg(), t).await.unwrap();
        assert_eq!((r.serve_pid, r.boot_ms), (9, 80));
        c.ping(t).await.unwrap();
        let err = c
            .call::<_, serde_json::Value>("init.bogus", &serde_json::json!({}), t)
            .await
            .unwrap_err();
        assert!(matches!(err, SandboxError::Protocol(_)), "{err}");
        c.shutdown(500, t).await.unwrap();
    }

    #[tokio::test]
    async fn silent_guest_times_out() {
        let (a, _b) = tokio::io::duplex(4096);
        let mut c = ControlClient::new(a);
        let err = c.ping(Duration::from_millis(50)).await.unwrap_err();
        assert!(matches!(err, SandboxError::Timeout { .. }), "{err}");
    }

    #[tokio::test]
    async fn connect_times_out_without_socket() {
        let dir = tempfile::tempdir().unwrap();
        let err = connect_control(
            &dir.path().join("ctl.sock"),
            Instant::now() + Duration::from_millis(60),
        )
        .await
        .unwrap_err();
        let msg = err.to_string();
        assert!(matches!(err, SandboxError::Timeout { .. }), "{msg}");
        assert!(msg.contains("attempt"), "{msg}");
    }

    /// libkrun accepts and immediately closes host connections until the guest listens: the
    /// ping probe must retry past those.
    #[tokio::test]
    async fn retries_past_accept_then_eof() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("ctl.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        tokio::spawn(async move {
            for _ in 0..3 {
                let (s, _) = listener.accept().await.unwrap();
                drop(s); // guest not listening yet
            }
            let (s, _) = listener.accept().await.unwrap();
            fake_init(s).await;
        });
        let (mut c, attempts) = connect_control(&sock, Instant::now() + Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(attempts, 4);
        c.configure(&init_cfg(), Duration::from_secs(1))
            .await
            .unwrap();
    }
}
