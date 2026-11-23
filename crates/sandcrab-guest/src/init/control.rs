//! The `init.*` control channel served by `crabd init` on vsock port 1.
//!
//! Requests on one control connection are handled **sequentially** (configure / export /
//! shutdown are rare and order-sensitive). The connection logic is generic over the stream
//! and over [`InitOps`], so it is tested over an in-memory duplex with a fake system.

use std::{io, path::PathBuf, sync::Arc, time::Duration};

use sandcrab_protocol::{
    METHOD_PING, ProtocolError, RpcMessage, RpcRequest, RpcResponse,
    init::{
        InitConfig, InitConfigureResult, InitExportAck, InitExportParams, InitShutdownParams,
        InitShutdownResult, METHOD_CONFIGURE, METHOD_EXPORT, METHOD_SHUTDOWN, archive,
    },
};
use serde::{Serialize, de::DeserializeOwned};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tracing::{debug, info, warn};

use crate::{codec, init::export, server::BoxFuture};

/// The privileged operations behind the control methods.
pub trait InitOps: Send + Sync + 'static {
    /// Apply the config (mounts, hostname, ...) and start `crabd serve`. Called at most once
    /// successfully; implementations reject a second call.
    fn configure(self: Arc<Self>, cfg: InitConfig) -> BoxFuture<io::Result<InitConfigureResult>>;
    /// Guest mount point of a configured **writable** drive.
    fn export_root(&self, drive_id: &str) -> io::Result<PathBuf>;
    /// Stop everything and power off. Only returns if powering off failed.
    fn shutdown(self: Arc<Self>, grace: Duration) -> BoxFuture<()>;
}

fn to_protocol(e: &io::Error) -> ProtocolError {
    match e.kind() {
        io::ErrorKind::InvalidInput => ProtocolError::InvalidParams(e.to_string()),
        io::ErrorKind::NotFound => ProtocolError::NotFound(e.to_string()),
        _ => ProtocolError::ExecutionFailed(e.to_string()),
    }
}

fn params<T: DeserializeOwned>(req: &RpcRequest) -> Result<T, ProtocolError> {
    let v = req
        .params
        .clone()
        .unwrap_or_else(|| serde_json::Value::Object(Default::default()));
    serde_json::from_value(v).map_err(|e| ProtocolError::InvalidParams(e.to_string()))
}

fn ok<T: Serialize>(req: &RpcRequest, v: &T) -> RpcResponse {
    match serde_json::to_value(v) {
        Ok(v) => RpcResponse::success(req.id.clone(), v),
        Err(e) => RpcResponse::failure(
            Some(req.id.clone()),
            &ProtocolError::Internal(e.to_string()),
        ),
    }
}

/// Serve one control connection until EOF, an export, or a shutdown.
pub async fn serve_control_connection<S>(stream: S, ops: Arc<dyn InitOps>) -> io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let mut stream = stream;
    loop {
        let payload = match codec::read_frame(&mut stream).await {
            Ok(Some(p)) => p,
            Ok(None) => return Ok(()),
            Err(e) => {
                debug!(error = %e, "control read ended");
                let _ =
                    codec::write_frame(&mut stream, &RpcResponse::failure(None, &e.into())).await;
                return Ok(());
            }
        };
        let req = match RpcMessage::from_slice(&payload) {
            Ok(RpcMessage::Request(r)) => r,
            Ok(_) => continue,
            Err(e) => {
                let id = RpcMessage::sniff_id(&payload);
                write(&mut stream, &RpcResponse::failure(id, &e)).await?;
                continue;
            }
        };
        debug!(method = %req.method, "control request");
        match req.method.as_str() {
            METHOD_PING => write(&mut stream, &ok(&req, &serde_json::json!({}))).await?,
            METHOD_CONFIGURE => {
                let resp = match params::<InitConfig>(&req) {
                    Ok(cfg) => match Arc::clone(&ops).configure(cfg).await {
                        Ok(r) => ok(&req, &r),
                        Err(e) => {
                            warn!(error = %e, "init.configure failed");
                            RpcResponse::failure(Some(req.id.clone()), &to_protocol(&e))
                        }
                    },
                    Err(e) => RpcResponse::failure(Some(req.id.clone()), &e),
                };
                write(&mut stream, &resp).await?;
            }
            METHOD_EXPORT => {
                let root = params::<InitExportParams>(&req)
                    .and_then(|p| ops.export_root(&p.drive_id).map_err(|e| to_protocol(&e)));
                let root = match root {
                    Ok(r) => r,
                    Err(e) => {
                        write(&mut stream, &RpcResponse::failure(Some(req.id.clone()), &e)).await?;
                        continue;
                    }
                };
                // Refuse an archive bigger than the drive while an error can still be sent.
                let checked = {
                    let root = root.clone();
                    tokio::task::spawn_blocking(move || export::precheck(&root))
                        .await
                        .unwrap_or_else(|e| Err(io::Error::other(e.to_string())))
                };
                if let Err(e) = checked {
                    warn!(root = %root.display(), error = %e, "export refused");
                    let err = to_protocol(&e);
                    write(
                        &mut stream,
                        &RpcResponse::failure(Some(req.id.clone()), &err),
                    )
                    .await?;
                    continue;
                }
                let ack = InitExportAck {
                    format: archive::FORMAT.to_owned(),
                    guest_path: root.clone(),
                };
                write(&mut stream, &ok(&req, &ack)).await?;
                // From here on the connection is a raw tar stream; an error simply truncates
                // it (the host detects the missing end-of-archive marker).
                match export::stream_tree(root.clone(), None, &mut stream).await {
                    Ok(stats) => info!(root = %root.display(), ?stats, "export finished"),
                    Err(e) => warn!(root = %root.display(), error = %e, "export aborted"),
                }
                let _ = stream.shutdown().await;
                return Ok(());
            }
            METHOD_SHUTDOWN => {
                let p = match params::<InitShutdownParams>(&req) {
                    Ok(p) => p,
                    Err(e) => {
                        write(&mut stream, &RpcResponse::failure(Some(req.id.clone()), &e)).await?;
                        continue;
                    }
                };
                // Ack first: once shutdown starts the VM may vanish before a reply is sent.
                let _ = write(&mut stream, &ok(&req, &InitShutdownResult {})).await;
                let _ = stream.shutdown().await;
                Arc::clone(&ops)
                    .shutdown(Duration::from_millis(p.grace_ms))
                    .await;
                return Ok(());
            }
            other => {
                let e = ProtocolError::MethodNotFound(other.to_owned());
                write(&mut stream, &RpcResponse::failure(Some(req.id.clone()), &e)).await?;
            }
        }
    }
}

async fn write<S: AsyncWrite + Unpin>(s: &mut S, resp: &RpcResponse) -> io::Result<()> {
    codec::write_frame(s, resp).await.map_err(|e| match e {
        sandcrab_protocol::FrameError::Io(e) => e,
        other => io::Error::other(other.to_string()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use sandcrab_protocol::{ResponsePayload, codes};
    use serde_json::json;
    use std::sync::Mutex;
    use tokio::io::{AsyncReadExt, DuplexStream};

    #[derive(Default)]
    struct Fake {
        configured: Mutex<Option<InitConfig>>,
        root: PathBuf,
        shutdowns: Mutex<Vec<Duration>>,
    }

    impl InitOps for Fake {
        fn configure(
            self: Arc<Self>,
            cfg: InitConfig,
        ) -> BoxFuture<io::Result<InitConfigureResult>> {
            let mut g = self.configured.lock().unwrap();
            let res = if g.is_some() {
                Err(io::Error::other("already configured"))
            } else if let Err(e) = crate::init::config::validate_config(cfg.clone()) {
                Err(e)
            } else {
                *g = Some(cfg);
                Ok(InitConfigureResult {
                    serve_pid: 42,
                    boot_ms: 7,
                })
            };
            Box::pin(async move { res })
        }
        fn export_root(&self, drive_id: &str) -> io::Result<PathBuf> {
            if drive_id == "ws0" {
                Ok(self.root.clone())
            } else {
                Err(io::Error::new(io::ErrorKind::NotFound, "no such drive"))
            }
        }
        fn shutdown(self: Arc<Self>, grace: Duration) -> BoxFuture<()> {
            self.shutdowns.lock().unwrap().push(grace);
            Box::pin(async {})
        }
    }

    fn start(fake: Arc<Fake>) -> (DuplexStream, tokio::task::JoinHandle<io::Result<()>>) {
        let (c, s) = tokio::io::duplex(1 << 16);
        let ops: Arc<dyn InitOps> = fake;
        (c, tokio::spawn(serve_control_connection(s, ops)))
    }

    async fn call(c: &mut DuplexStream, method: &str, p: serde_json::Value) -> RpcResponse {
        codec::write_frame(c, &RpcRequest::new(1, method, Some(p)))
            .await
            .unwrap();
        let f = codec::read_frame(c).await.unwrap().unwrap();
        serde_json::from_slice(&f).unwrap()
    }

    fn cfg_json() -> serde_json::Value {
        json!({
            "sandbox_id": "sb", "hostname": "box",
            "user": {"uid": 1000, "gid": 1000},
            "default_cwd": "/workspace",
            "mounts": [{"drive_id": "ws0", "guest_path": "/workspace"}]
        })
    }

    #[tokio::test]
    async fn configure_once_then_ping_and_unknown() {
        let fake = Arc::new(Fake::default());
        let (mut c, _h) = start(Arc::clone(&fake));
        let r = call(&mut c, METHOD_CONFIGURE, cfg_json()).await;
        let v = r.into_result().unwrap();
        assert_eq!(v["serve_pid"], 42);
        let r = call(&mut c, METHOD_CONFIGURE, cfg_json()).await;
        assert!(r.into_result().is_err());
        assert!(call(&mut c, "ping", json!({})).await.into_result().is_ok());
        let ResponsePayload::Error(e) = call(&mut c, "bash", json!({})).await.payload else {
            panic!()
        };
        assert_eq!(e.code, codes::METHOD_NOT_FOUND);
    }

    #[tokio::test]
    async fn configure_rejects_invalid_params() {
        let (mut c, _h) = start(Arc::new(Fake::default()));
        let mut bad = cfg_json();
        bad["mounts"][0]["guest_path"] = json!("/proc/evil");
        let ResponsePayload::Error(e) = call(&mut c, METHOD_CONFIGURE, bad).await.payload else {
            panic!()
        };
        assert_eq!(e.code, codes::INVALID_PARAMS);
        let ResponsePayload::Error(e) = call(&mut c, METHOD_CONFIGURE, json!({"nope": 1}))
            .await
            .payload
        else {
            panic!()
        };
        assert_eq!(e.code, codes::INVALID_PARAMS);
    }

    #[tokio::test]
    async fn export_acks_then_streams_tar_to_eof() {
        let t = tempfile::tempdir().unwrap();
        std::fs::write(t.path().join("hello.txt"), b"hi").unwrap();
        let fake = Arc::new(Fake {
            root: t.path().to_owned(),
            ..Default::default()
        });
        let (mut c, h) = start(fake);
        let r = call(&mut c, METHOD_EXPORT, json!({"drive_id": "nope"})).await;
        assert!(
            r.into_result().is_err(),
            "unknown drive → error, conn stays"
        );
        let ack: InitExportAck = serde_json::from_value(
            call(&mut c, METHOD_EXPORT, json!({"drive_id": "ws0"}))
                .await
                .into_result()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(ack.format, archive::FORMAT);
        let mut tar_bytes = Vec::new();
        c.read_to_end(&mut tar_bytes).await.unwrap();
        h.await.unwrap().unwrap();
        let mut a = tar::Archive::new(&tar_bytes[..]);
        let names: Vec<_> = a
            .entries()
            .unwrap()
            .map(|e| e.unwrap().path().unwrap().display().to_string())
            .collect();
        assert_eq!(names, ["hello.txt"]);
    }

    #[tokio::test]
    async fn shutdown_acks_then_calls_ops() {
        let fake = Arc::new(Fake::default());
        let (mut c, h) = start(Arc::clone(&fake));
        let r = call(&mut c, METHOD_SHUTDOWN, json!({"grace_ms": 250})).await;
        assert!(r.into_result().is_ok());
        h.await.unwrap().unwrap();
        assert_eq!(
            *fake.shutdowns.lock().unwrap(),
            [Duration::from_millis(250)]
        );
    }
}
