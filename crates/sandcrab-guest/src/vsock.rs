//! virtio-vsock listener (Linux only), used by `crabd serve --vsock` and `crabd init`.
//!
//! Binds `VMADDR_CID_ANY:<port>`: inside a Firecracker guest the host reaches it through the
//! VMM's Unix socket (`CONNECT <port>\n`). Streams are plain `AsyncRead + AsyncWrite`, so the
//! same framing / server code runs on top.

use std::io;

#[cfg(target_os = "linux")]
pub use tokio_vsock::{VMADDR_CID_ANY, VMADDR_CID_LOCAL, VsockAddr, VsockListener, VsockStream};

/// Bind a vsock listener on `port` for any CID.
#[cfg(target_os = "linux")]
pub fn bind(port: u32) -> io::Result<VsockListener> {
    VsockListener::bind(VsockAddr::new(VMADDR_CID_ANY, port)).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("bind vsock port {port}: {e} (is the vsock transport loaded?)"),
        )
    })
}

/// The error returned for `--vsock` on a platform without AF_VSOCK.
#[must_use]
pub fn unsupported(port: u32) -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        format!("vsock transport (port {port}) requires Linux; use --unix or --tcp"),
    )
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use crate::{Server, ServerConfig, ToolContext, server::ListenAddr};
    use sandcrab_protocol::{RpcRequest, RpcResponse};
    use std::sync::Arc;

    /// Needs the `vsock_loopback` transport (CID 1): `sudo modprobe vsock_loopback`.
    /// Not loaded on the CI VPS (no root), hence ignored by default.
    #[tokio::test]
    #[ignore = "requires the vsock_loopback kernel module"]
    async fn serve_over_vsock_loopback() {
        let port = 40_000 + (std::process::id() % 10_000);
        let server = Server::bind(
            ServerConfig::new(ListenAddr::Vsock(port)),
            Arc::new(ToolContext::new("/tmp")),
        )
        .await
        .unwrap();
        assert_eq!(server.endpoint(), format!("vsock:{port}"));
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let h = tokio::spawn(server.run(async {
            let _ = stop_rx.await;
        }));
        let mut s = VsockStream::connect(VsockAddr::new(VMADDR_CID_LOCAL, port))
            .await
            .unwrap();
        crate::codec::write_frame(&mut s, &RpcRequest::new(1, "ping", None))
            .await
            .unwrap();
        let f = crate::codec::read_frame(&mut s).await.unwrap().unwrap();
        let resp: RpcResponse = serde_json::from_slice(&f).unwrap();
        assert!(resp.into_result().is_ok());
        drop(s);
        stop_tx.send(()).unwrap();
        h.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn bind_error_mentions_port() {
        // Without any vsock transport the bind fails; with one it succeeds. Either way the
        // call must not panic, and a failure names the port.
        match bind(u32::MAX - 7) {
            Ok(_) => {}
            Err(e) => assert!(e.to_string().contains("vsock port"), "{e}"),
        }
    }
}
