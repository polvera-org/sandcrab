//! Host client for the guest `init.*` control channel (vsock port 1, design §3).
//!
//! Requests on one connection are sequential. `init.export` consumes the connection: after
//! its ack the stream carries raw `sandcrab-tar-v1` bytes until EOF.

use std::time::{Duration, Instant};

use sandcrab_core::{Result, SandboxError};
use sandcrab_protocol::init::{
    InitConfig, InitConfigureResult, InitExportAck, InitExportParams, InitShutdownParams,
    InitShutdownResult, METHOD_CONFIGURE, METHOD_EXPORT, METHOD_SHUTDOWN, archive,
};
use sandcrab_protocol::{
    HEADER_LEN, METHOD_PING, ProtocolError, RpcMessage, RpcRequest, encode_frame, parse_header,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::vsock::vsock_connect;

/// One control connection.
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

    /// Request shutdown; the guest acks and then powers off (the VMM exits).
    pub async fn shutdown(&mut self, grace_ms: u64, timeout: Duration) -> Result<()> {
        self.call::<_, InitShutdownResult>(
            METHOD_SHUTDOWN,
            &InitShutdownParams { grace_ms },
            timeout,
        )
        .await
        .map(drop)
    }

    /// Start exporting `drive_id`; returns the ack and the stream positioned at the first tar
    /// byte.
    pub async fn export(mut self, drive_id: &str, timeout: Duration) -> Result<(InitExportAck, S)> {
        let ack: InitExportAck = self
            .call(
                METHOD_EXPORT,
                &InitExportParams {
                    drive_id: drive_id.to_owned(),
                },
                timeout,
            )
            .await?;
        if ack.format != archive::FORMAT {
            return Err(ProtocolError::Internal(format!(
                "guest exports `{}`, host expects `{}`",
                ack.format,
                archive::FORMAT
            ))
            .into());
        }
        Ok((ack, self.stream))
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

/// Connect to the control port, retrying with the design §4 backoff (2 ms ×1.5, cap 20 ms)
/// until `deadline`. Returns the connected client and the number of attempts.
pub async fn connect_control(
    uds: &std::path::Path,
    port: u32,
    deadline: Instant,
) -> Result<(ControlClient<tokio::net::UnixStream>, u32)> {
    let started = Instant::now();
    let mut backoff = Duration::from_millis(2);
    let mut attempts = 0u32;
    loop {
        attempts += 1;
        let remaining = deadline.saturating_duration_since(Instant::now());
        let err = match tokio::time::timeout(remaining, vsock_connect(uds, port)).await {
            Ok(Ok(s)) => return Ok((ControlClient::new(s), attempts)),
            Ok(Err(e)) => e.to_string(),
            Err(_) => "connect timed out".to_owned(),
        };
        if Instant::now() >= deadline {
            return Err(SandboxError::Timeout {
                what: format!(
                    "guest control port {port} via {} after {attempts} attempt(s); last error: {err}",
                    uds.display()
                ),
                after: started.elapsed(),
            });
        }
        tokio::time::sleep(backoff.min(deadline.saturating_duration_since(Instant::now()))).await;
        backoff = backoff.mul_f64(1.5).min(Duration::from_millis(20));
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use sandcrab_protocol::RpcResponse;
    use sandcrab_protocol::init::{GuestMount, InitUser};

    use super::*;

    fn cfg() -> InitConfig {
        InitConfig {
            sandbox_id: "sb".into(),
            hostname: "sc-1".into(),
            user: InitUser {
                uid: 1000,
                gid: 1000,
                home: None,
                name: None,
            },
            env: Default::default(),
            default_cwd: "/workspace".into(),
            mounts: vec![GuestMount::block("ws0", None, "/workspace", false)],
            network: false,
            dns: vec![],
            tools_port: 1024,
            host_time_unix_ms: Some(1),
        }
    }

    /// Minimal guest: answers configure/ping, then an export with ack + raw bytes + EOF.
    async fn fake_guest(mut s: tokio::io::DuplexStream) {
        loop {
            let Ok(payload) = read_frame(&mut s).await else {
                return;
            };
            let RpcMessage::Request(req) = RpcMessage::from_slice(&payload).unwrap() else {
                continue;
            };
            let result = match req.method.as_str() {
                METHOD_CONFIGURE => {
                    let c: InitConfig = serde_json::from_value(req.params.unwrap()).unwrap();
                    assert_eq!(
                        c.mounts[0],
                        GuestMount::block("ws0", None, "/workspace", false)
                    );
                    serde_json::json!({"serve_pid": 9, "boot_ms": 120})
                }
                METHOD_PING => serde_json::json!({}),
                METHOD_EXPORT => {
                    let ack =
                        serde_json::json!({"format": archive::FORMAT, "guest_path": "/workspace"});
                    let f = encode_frame(&RpcResponse::success(req.id, ack)).unwrap();
                    s.write_all(&f).await.unwrap();
                    s.write_all(b"TARBYTES").await.unwrap();
                    return;
                }
                _ => {
                    let f = encode_frame(&RpcResponse::failure(
                        Some(req.id),
                        &ProtocolError::MethodNotFound(req.method.clone()),
                    ))
                    .unwrap();
                    s.write_all(&f).await.unwrap();
                    continue;
                }
            };
            let f = encode_frame(&RpcResponse::success(req.id, result)).unwrap();
            s.write_all(&f).await.unwrap();
        }
    }

    #[tokio::test]
    async fn configure_ping_export() {
        let (a, b) = tokio::io::duplex(4096);
        tokio::spawn(fake_guest(b));
        let mut c = ControlClient::new(a);
        let t = Duration::from_secs(1);
        let r = c.configure(&cfg(), t).await.unwrap();
        assert_eq!((r.serve_pid, r.boot_ms), (9, 120));
        c.ping(t).await.unwrap();
        let err = c
            .call::<_, serde_json::Value>("init.bogus", &serde_json::json!({}), t)
            .await
            .unwrap_err();
        assert!(matches!(err, SandboxError::Protocol(_)), "{err}");
        let (ack, mut s) = c.export("ws0", t).await.unwrap();
        assert_eq!(ack.guest_path, PathBuf::from("/workspace"));
        let mut rest = Vec::new();
        s.read_to_end(&mut rest).await.unwrap();
        assert_eq!(rest, b"TARBYTES");
    }

    #[tokio::test]
    async fn silent_guest_times_out() {
        let (a, _b) = tokio::io::duplex(4096);
        let mut c = ControlClient::new(a);
        let err = c.ping(Duration::from_millis(50)).await.unwrap_err();
        assert!(matches!(err, SandboxError::Timeout { .. }), "{err}");
    }

    #[tokio::test]
    async fn connect_control_times_out_without_vmm() {
        let dir = tempfile::tempdir().unwrap();
        let err = connect_control(
            &dir.path().join("v.sock"),
            1,
            Instant::now() + Duration::from_millis(60),
        )
        .await
        .unwrap_err();
        let msg = err.to_string();
        assert!(matches!(err, SandboxError::Timeout { .. }), "{msg}");
        assert!(msg.contains("attempt"), "{msg}");
    }
}
