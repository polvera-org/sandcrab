//! `tunnel.open` against an in-process `crabd` on a temp Unix socket, with a TCP echo
//! server on the "guest" loopback.

use std::{net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

use sandcrab_guest::{ListenAddr, Server, ServerConfig, ToolContext, TunnelConfig, codec};
use sandcrab_protocol::{RpcResponse, TunnelOpenParams, codes};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, UnixStream},
    sync::oneshot,
    task::JoinHandle,
};

struct TestServer {
    _dir: TempDir,
    sock: PathBuf,
    stop: Option<oneshot::Sender<()>>,
    handle: JoinHandle<std::io::Result<()>>,
}

impl TestServer {
    async fn start(tweak: impl FnOnce(&mut ServerConfig)) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("crabd.sock");
        let mut config = ServerConfig::new(ListenAddr::Unix(sock.clone()));
        config.shutdown_grace = Duration::from_millis(500);
        tweak(&mut config);
        let server = Server::bind(config, Arc::new(ToolContext::new(dir.path())))
            .await
            .unwrap();
        let (tx, rx) = oneshot::channel::<()>();
        let handle = tokio::spawn(server.run(async {
            let _ = rx.await;
        }));
        Self {
            _dir: dir,
            sock,
            stop: Some(tx),
            handle,
        }
    }

    async fn connect(&self) -> UnixStream {
        UnixStream::connect(&self.sock).await.unwrap()
    }

    /// Open a tunnel; returns the stream and the response frame.
    async fn tunnel(&self, port: u16) -> (UnixStream, RpcResponse) {
        let mut s = self.connect().await;
        let req = TunnelOpenParams { port }.to_request(1);
        codec::write_frame(&mut s, &req).await.unwrap();
        let resp = recv(&mut s).await;
        (s, resp)
    }

    async fn shutdown(mut self) {
        self.stop.take().unwrap().send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), self.handle)
            .await
            .expect("server stops")
            .unwrap()
            .unwrap();
    }
}

async fn recv<S: tokio::io::AsyncRead + Unpin>(s: &mut S) -> RpcResponse {
    let payload = tokio::time::timeout(Duration::from_secs(5), codec::read_frame(s))
        .await
        .expect("frame within 5s")
        .unwrap()
        .expect("a frame, not EOF");
    serde_json::from_slice(&payload).unwrap()
}

fn error_code(resp: &RpcResponse) -> i64 {
    match &resp.payload {
        sandcrab_protocol::ResponsePayload::Error(e) => e.code,
        sandcrab_protocol::ResponsePayload::Result(r) => panic!("expected error, got {r}"),
    }
}

fn result(resp: &RpcResponse) -> &Value {
    match &resp.payload {
        sandcrab_protocol::ResponsePayload::Result(r) => r,
        sandcrab_protocol::ResponsePayload::Error(e) => panic!("expected result, got {e:?}"),
    }
}

/// Loopback echo server: echoes until EOF, then half-closes.
async fn echo_server() -> SocketAddr {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (s, _) = l.accept().await.unwrap();
            tokio::spawn(async move {
                let (mut r, mut w) = s.into_split();
                let _ = tokio::io::copy(&mut r, &mut w).await;
                let _ = w.shutdown().await;
            });
        }
    });
    addr
}

/// A loopback port with nothing listening.
async fn dead_port() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    l.local_addr().unwrap().port()
}

async fn assert_eof(s: &mut UnixStream) {
    let mut buf = [0u8; 16];
    let n = tokio::time::timeout(Duration::from_secs(5), s.read(&mut buf))
        .await
        .expect("closed within 5s")
        .unwrap_or(0);
    assert_eq!(n, 0, "expected EOF");
}

#[tokio::test]
async fn ack_then_raw_bytes_and_half_close() {
    let echo = echo_server().await;
    let srv = TestServer::start(|_| {}).await;
    let (mut s, resp) = srv.tunnel(echo.port()).await;
    assert_eq!(resp.id, Some(1.into()));
    assert_eq!(result(&resp), &json!({"port": echo.port()}));

    // Raw bytes, not frames: a valid-looking frame is echoed back verbatim.
    let data = b"\x00\x00\x00\x02{}hello";
    s.write_all(data).await.unwrap();
    s.shutdown().await.unwrap(); // half-close host → guest
    let mut back = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), s.read_to_end(&mut back))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(back, data);
    srv.shutdown().await;
}

#[tokio::test]
async fn refused_and_invalid_targets() {
    let srv = TestServer::start(|_| {}).await;

    let (mut s, resp) = srv.tunnel(dead_port().await).await;
    assert_eq!(error_code(&resp), codes::NOT_FOUND);
    assert_eof(&mut s).await;

    let (mut s, resp) = srv.tunnel(0).await;
    assert_eq!(error_code(&resp), codes::INVALID_PARAMS);
    assert_eof(&mut s).await;

    // A host field is rejected: the target is always guest loopback.
    let mut s = srv.connect().await;
    let req = json!({"jsonrpc":"2.0","id":9,"method":"tunnel.open","params":{"port":80,"host":"10.0.0.1"}});
    codec::write_frame(&mut s, &req).await.unwrap();
    assert_eq!(error_code(&recv(&mut s).await), codes::INVALID_PARAMS);
    assert_eof(&mut s).await;
    srv.shutdown().await;
}

#[tokio::test]
async fn tunnel_open_mid_connection_is_rejected() {
    let echo = echo_server().await;
    let srv = TestServer::start(|_| {}).await;
    let mut s = srv.connect().await;
    codec::write_frame(&mut s, &json!({"jsonrpc":"2.0","id":1,"method":"ping"}))
        .await
        .unwrap();
    assert_eq!(result(&recv(&mut s).await), &json!({}));
    let req = TunnelOpenParams { port: echo.port() }.to_request(2);
    codec::write_frame(&mut s, &req).await.unwrap();
    let resp = recv(&mut s).await;
    assert_eq!(resp.id, Some(2.into()));
    assert_eq!(error_code(&resp), codes::INVALID_REQUEST);
    // Still in frame mode.
    codec::write_frame(&mut s, &json!({"jsonrpc":"2.0","id":3,"method":"ping"}))
        .await
        .unwrap();
    assert_eq!(recv(&mut s).await.id, Some(3.into()));
    srv.shutdown().await;
}

#[tokio::test]
async fn tunnel_limit_is_enforced_and_released() {
    let echo = echo_server().await;
    let srv = TestServer::start(|c| {
        c.tunnels = TunnelConfig {
            max_tunnels: 2,
            ..TunnelConfig::default()
        };
    })
    .await;
    let (a, ra) = srv.tunnel(echo.port()).await;
    let (_b, rb) = srv.tunnel(echo.port()).await;
    result(&ra);
    result(&rb);
    let (mut c, rc) = srv.tunnel(echo.port()).await;
    assert_eq!(error_code(&rc), codes::EXECUTION_FAILED);
    assert_eof(&mut c).await;

    drop(a);
    // The slot frees once the relay notices the close.
    let mut ok = false;
    for _ in 0..50 {
        let (_d, rd) = srv.tunnel(echo.port()).await;
        if matches!(rd.payload, sandcrab_protocol::ResponsePayload::Result(_)) {
            ok = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(ok, "slot released after a tunnel closed");
    srv.shutdown().await;
}

#[tokio::test]
async fn tunnels_do_not_use_connection_slots() {
    let echo = echo_server().await;
    let srv = TestServer::start(|c| c.max_connections = 1).await;
    let (mut t1, r1) = srv.tunnel(echo.port()).await;
    result(&r1);
    let (_t2, r2) = srv.tunnel(echo.port()).await;
    result(&r2);
    // A tool connection is still served.
    let mut s = srv.connect().await;
    codec::write_frame(&mut s, &json!({"jsonrpc":"2.0","id":1,"method":"ping"}))
        .await
        .unwrap();
    assert_eq!(recv(&mut s).await.id, Some(1.into()));
    // And the tunnels still relay.
    t1.write_all(b"ping").await.unwrap();
    let mut buf = [0u8; 4];
    t1.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"ping");
    srv.shutdown().await;
}

#[tokio::test]
async fn shutdown_closes_open_tunnels() {
    let echo = echo_server().await;
    let srv = TestServer::start(|_| {}).await;
    let (mut s, resp) = srv.tunnel(echo.port()).await;
    result(&resp);
    srv.shutdown().await;
    assert_eof(&mut s).await;
}

/// Loopback server that sends `bye`, half-closes, then reports everything it reads
/// afterwards on the channel (once the client side is closed).
async fn half_closing_server() -> (SocketAddr, tokio::sync::mpsc::Receiver<Vec<u8>>) {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let (tx, rx) = tokio::sync::mpsc::channel(4);
    tokio::spawn(async move {
        loop {
            let (s, _) = l.accept().await.unwrap();
            let tx = tx.clone();
            tokio::spawn(async move {
                let (mut r, mut w) = s.into_split();
                w.write_all(b"bye").await.unwrap();
                w.shutdown().await.unwrap();
                let mut seen = Vec::new();
                let _ = r.read_to_end(&mut seen).await;
                let _ = tx.send(seen).await;
            });
        }
    });
    (addr, rx)
}

/// Unix-socket transport (Docker): the target's half-close reaches the host as EOF and the
/// host can keep sending.
#[tokio::test]
async fn target_half_close_is_propagated_by_default() {
    let (target, mut seen) = half_closing_server().await;
    let srv = TestServer::start(|_| {}).await;
    let (mut s, resp) = srv.tunnel(target.port()).await;
    result(&resp);
    let mut got = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), s.read_to_end(&mut got))
        .await
        .expect("EOF within 5s")
        .unwrap();
    assert_eq!(got, b"bye");
    s.write_all(b"after").await.unwrap();
    s.shutdown().await.unwrap();
    let after = tokio::time::timeout(Duration::from_secs(5), seen.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        after, b"after",
        "host → guest still open after the target's FIN"
    );
    srv.shutdown().await;
}

/// vsock policy: the target's EOF flushes its bytes and closes the whole tunnel, so a
/// read-until-close client finishes even if the transport drops half-close.
#[tokio::test]
async fn close_on_target_eof_flushes_then_closes() {
    let (target, mut seen) = half_closing_server().await;
    let srv = TestServer::start(|c| c.tunnels.close_on_target_eof = true).await;
    let (mut s, resp) = srv.tunnel(target.port()).await;
    result(&resp);
    let mut got = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), s.read_to_end(&mut got))
        .await
        .expect("EOF within 5s")
        .unwrap();
    assert_eq!(got, b"bye");
    // The tunnel is fully closed: the target's read side ends with nothing more.
    let after = tokio::time::timeout(Duration::from_secs(5), seen.recv())
        .await
        .expect("target sees the close")
        .unwrap();
    assert!(after.is_empty(), "{after:?}");

    // Large payloads are flushed before the close; host EOF still reaches the target.
    let echo = echo_server().await;
    let (mut s, resp) = srv.tunnel(echo.port()).await;
    result(&resp);
    let data = vec![5u8; 1 << 20];
    let (mut r, mut w) = s.split();
    let send = async {
        w.write_all(&data).await.unwrap();
        w.shutdown().await.unwrap();
    };
    let mut back = Vec::new();
    let recv = tokio::time::timeout(Duration::from_secs(10), r.read_to_end(&mut back));
    let ((), res) = tokio::join!(send, recv);
    res.unwrap().unwrap();
    assert_eq!(back.len(), data.len());
    srv.shutdown().await;
}
