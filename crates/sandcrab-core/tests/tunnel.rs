//! End-to-end port forwarding: host [`PortForwarder`] on 127.0.0.1 → Unix socket → in-process
//! `crabd` server → `tunnel.open` → TCP echo server on the "guest" loopback.

use std::{net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

use sandcrab_core::{
    Connector, Endpoint, ForwarderConfig, PortForwarder, SandboxError, open_tunnel,
};
use sandcrab_guest::{ListenAddr, Server, ServerConfig, ToolContext};
use tempfile::TempDir;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::oneshot,
    task::JoinHandle,
};

const TIMEOUT: Duration = Duration::from_secs(10);

struct Crabd {
    _dir: TempDir,
    sock: PathBuf,
    stop: Option<oneshot::Sender<()>>,
    handle: JoinHandle<std::io::Result<()>>,
}

impl Crabd {
    async fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("crabd.sock");
        let mut config = ServerConfig::new(ListenAddr::Unix(sock.clone()));
        config.shutdown_grace = Duration::from_millis(500);
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

    fn connector(&self) -> Arc<dyn Connector> {
        Arc::new(Endpoint::Unix(self.sock.clone()))
    }

    async fn stop(mut self) {
        self.stop.take().unwrap().send(()).unwrap();
        tokio::time::timeout(TIMEOUT, self.handle)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
}

/// Loopback echo server: echoes until EOF, then half-closes its write side.
async fn echo_server() -> SocketAddr {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((s, _)) = l.accept().await {
            tokio::spawn(async move {
                let (mut r, mut w) = s.into_split();
                let _ = tokio::io::copy(&mut r, &mut w).await;
                let _ = w.shutdown().await;
            });
        }
    });
    addr
}

fn payload(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

/// Write all of `data` while concurrently reading the echo to EOF (avoids deadlock on full
/// buffers), half-closing after the write.
async fn round_trip(stream: TcpStream, data: Vec<u8>) -> Vec<u8> {
    let (mut r, mut w) = stream.into_split();
    let writer = tokio::spawn(async move {
        w.write_all(&data).await.unwrap();
        w.shutdown().await.unwrap();
    });
    let mut back = Vec::new();
    tokio::time::timeout(TIMEOUT, r.read_to_end(&mut back))
        .await
        .expect("echo within timeout")
        .unwrap();
    writer.await.unwrap();
    back
}

#[tokio::test]
async fn large_payload_round_trip() {
    let echo = echo_server().await;
    let crabd = Crabd::start().await;
    let fwd = PortForwarder::bind(
        crabd.connector(),
        echo.port(),
        0,
        ForwarderConfig::default(),
    )
    .await
    .unwrap();
    assert_eq!(fwd.local_addr().ip(), std::net::Ipv4Addr::LOCALHOST);

    let data = payload(32 * 1024 * 1024 + 17, 7);
    let s = TcpStream::connect(fwd.local_addr()).await.unwrap();
    let back = round_trip(s, data.clone()).await;
    assert_eq!(back.len(), data.len());
    assert!(back == data, "payload corrupted in transit");

    fwd.shutdown().await;
    crabd.stop().await;
}

#[tokio::test]
async fn concurrent_tunnels_are_independent() {
    let echo = echo_server().await;
    let crabd = Crabd::start().await;
    let fwd = PortForwarder::bind(
        crabd.connector(),
        echo.port(),
        0,
        ForwarderConfig::default(),
    )
    .await
    .unwrap();
    let addr = fwd.local_addr();
    let mut tasks = Vec::new();
    for i in 0..16u8 {
        tasks.push(tokio::spawn(async move {
            let data = payload(256 * 1024 + usize::from(i), i);
            let s = TcpStream::connect(addr).await.unwrap();
            let back = round_trip(s, data.clone()).await;
            assert!(back == data, "tunnel {i} corrupted");
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    fwd.shutdown().await;
    crabd.stop().await;
}

#[tokio::test]
async fn guest_close_reaches_host_client() {
    // Guest server sends a greeting and closes without reading.
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (mut s, _) = l.accept().await.unwrap();
        s.write_all(b"bye").await.unwrap();
    });
    let crabd = Crabd::start().await;
    let fwd = PortForwarder::bind(crabd.connector(), port, 0, ForwarderConfig::default())
        .await
        .unwrap();
    let mut s = TcpStream::connect(fwd.local_addr()).await.unwrap();
    let mut got = Vec::new();
    tokio::time::timeout(TIMEOUT, s.read_to_end(&mut got))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(got, b"bye");
    fwd.shutdown().await;
    crabd.stop().await;
}

#[tokio::test]
async fn refused_target_closes_client_and_typed_error() {
    let dead = {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        l.local_addr().unwrap().port()
    };
    let crabd = Crabd::start().await;

    let err = open_tunnel(crabd.connector().as_ref(), dead, TIMEOUT)
        .await
        .err()
        .expect("refused");
    assert!(
        matches!(
            err,
            SandboxError::Protocol(sandcrab_protocol::ProtocolError::NotFound(_))
        ),
        "{err:?}"
    );

    let fwd = PortForwarder::bind(crabd.connector(), dead, 0, ForwarderConfig::default())
        .await
        .unwrap();
    let mut s = TcpStream::connect(fwd.local_addr()).await.unwrap();
    let mut buf = [0u8; 8];
    let n = tokio::time::timeout(TIMEOUT, s.read(&mut buf))
        .await
        .unwrap()
        .unwrap_or(0);
    assert_eq!(n, 0, "client sees the connection close");
    fwd.shutdown().await;
    crabd.stop().await;
}

#[tokio::test]
async fn forwarder_limit_closes_excess_connections() {
    let echo = echo_server().await;
    let crabd = Crabd::start().await;
    let cfg = ForwarderConfig {
        max_tunnels: 1,
        ..ForwarderConfig::default()
    };
    let fwd = PortForwarder::bind(crabd.connector(), echo.port(), 0, cfg)
        .await
        .unwrap();
    let mut a = TcpStream::connect(fwd.local_addr()).await.unwrap();
    a.write_all(b"x").await.unwrap();
    let mut one = [0u8; 1];
    a.read_exact(&mut one).await.unwrap();
    assert_eq!(fwd.active_tunnels(), 1);

    let mut b = TcpStream::connect(fwd.local_addr()).await.unwrap();
    let mut buf = [0u8; 1];
    let n = tokio::time::timeout(TIMEOUT, b.read(&mut buf))
        .await
        .unwrap()
        .unwrap_or(0);
    assert_eq!(n, 0, "excess connection closed");
    fwd.shutdown().await;
    crabd.stop().await;
}

#[tokio::test]
async fn shutdown_closes_open_tunnels_and_listener() {
    let echo = echo_server().await;
    let crabd = Crabd::start().await;
    let fwd = PortForwarder::bind(
        crabd.connector(),
        echo.port(),
        0,
        ForwarderConfig::default(),
    )
    .await
    .unwrap();
    let addr = fwd.local_addr();
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(b"hi").await.unwrap();
    let mut two = [0u8; 2];
    s.read_exact(&mut two).await.unwrap();

    fwd.shutdown().await;
    let mut buf = [0u8; 1];
    let n = tokio::time::timeout(TIMEOUT, s.read(&mut buf))
        .await
        .unwrap()
        .unwrap_or(0);
    assert_eq!(n, 0, "open tunnel closed on shutdown");
    assert!(TcpStream::connect(addr).await.is_err(), "listener closed");
    crabd.stop().await;
}

#[tokio::test]
async fn crabd_shutdown_closes_tunnels() {
    let echo = echo_server().await;
    let crabd = Crabd::start().await;
    let fwd = PortForwarder::bind(
        crabd.connector(),
        echo.port(),
        0,
        ForwarderConfig::default(),
    )
    .await
    .unwrap();
    let mut s = TcpStream::connect(fwd.local_addr()).await.unwrap();
    s.write_all(b"hi").await.unwrap();
    let mut two = [0u8; 2];
    s.read_exact(&mut two).await.unwrap();

    crabd.stop().await; // sandbox destroyed
    let mut buf = [0u8; 1];
    let n = tokio::time::timeout(TIMEOUT, s.read(&mut buf))
        .await
        .unwrap()
        .unwrap_or(0);
    assert_eq!(n, 0, "tunnel closed when the guest goes away");
    fwd.shutdown().await;
}
